//! The boot comparison reads only lagging checkpoints, across a whole tenant.
use super::*;

#[tokio::test]
async fn boot_projection_comparison_is_batched_and_respects_effective_sequences() {
    let store = make_store("boot-projection-comparison").await;
    for tenant in ["installed-app", "other-tenant"] {
        for (ty, id, projected, journal) in [
            ("Payment", "current", 2, 2),
            ("Payment", "behind", 1, 2),
            ("Other", "behind", 1, 3),
            ("Payment", "missing", 0, 1),
            ("Payment", "counter-current", 1, 3),
            ("Payment", "snapshot", 1, 1),
            ("Payment", "deleted", 1, 1),
            ("Payment", "removed", 0, 1),
        ] {
            let pid = format!("{tenant}:{ty}:{id}");
            let events: Vec<_> = (0..journal)
                .map(|_| test_envelope("Touch", serde_json::json!({})))
                .collect();
            store.append(&pid, 0, &events).await.unwrap();
            if projected != 0 {
                store
                    .upsert_query_projection_with_state(
                        tenant,
                        ty,
                        id,
                        "Reviewing",
                        &serde_json::json!({}),
                        &serde_json::json!({}),
                        projected,
                    )
                    .await
                    .unwrap();
            }
        }
        for id in ["deleted", "removed"] {
            store
                .append(
                    &format!("{tenant}:Payment:{id}"),
                    1,
                    &[tombstone_envelope()],
                )
                .await
                .unwrap();
        }
        // A compacted head newer than the retained journal must also be repaired.
        store
            .save_snapshot(&format!("{tenant}:Payment:snapshot"), 4, b"{}")
            .await
            .unwrap();
        let conn = store
            .write_connection("test", super::super::WritePriority::High)
            .await
            .unwrap();
        conn.execute("INSERT INTO entity_catalog_counters (tenant, entity_type, entity_id, sequence_nr, counters)
            VALUES (?1, 'Payment', 'counter-current', 3, '{}')", params![tenant]).await.unwrap();
    }
    let gaps = store
        .lagging_query_projections("installed-app")
        .await
        .unwrap();
    let observed: Vec<_> = gaps
        .iter()
        .map(|g| {
            (
                g.journal.entity_type.as_str(),
                g.journal.entity_id.as_str(),
                g.projection.as_ref().map(|p| p.sequence_nr),
                g.journal.sequence_nr,
            )
        })
        .collect();
    assert_eq!(
        observed,
        vec![
            ("Other", "behind", Some(1), 3),
            ("Payment", "behind", Some(1), 2),
            ("Payment", "deleted", Some(1), 2),
            ("Payment", "missing", None, 1),
            ("Payment", "snapshot", Some(1), 4),
        ]
    );
    assert!(
        gaps.iter()
            .filter_map(|g| g.projection.as_ref())
            .all(|p| p.status == "Reviewing")
    );
    assert_eq!(
        store
            .latest_sequence_nr("installed-app:Payment:snapshot")
            .await
            .unwrap(),
        4
    );
    assert_eq!(
        store
            .latest_sequence_nr("installed-app:Payment:behind")
            .await
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .latest_sequence_nr("installed-app:Payment:absent")
            .await
            .unwrap(),
        0
    );
    let heads = store.load_journal_heads("installed-app").await.unwrap();
    assert_eq!(heads.len(), 8);
    assert_eq!(
        heads
            .iter()
            .find(|h| h.entity_id == "snapshot")
            .unwrap()
            .sequence_nr,
        4
    );
    assert!(
        heads
            .iter()
            .find(|h| h.entity_id == "removed")
            .unwrap()
            .deleted
    );
    let checkpoints = store
        .query_projection_checkpoints("installed-app")
        .await
        .unwrap();
    assert_eq!(
        checkpoints
            .iter()
            .find(|p| p.entity_id == "counter-current")
            .unwrap()
            .sequence_nr,
        3
    );
    // The old row sequence logged for a later split must include the counters.
    store
        .append(
            "installed-app:Payment:counter-current",
            3,
            &[test_envelope("Touch", serde_json::json!({}))],
        )
        .await
        .unwrap();
    let gaps = store
        .lagging_query_projections("installed-app")
        .await
        .unwrap();
    let counter = gaps
        .iter()
        .find(|g| g.journal.entity_id == "counter-current")
        .unwrap();
    assert_eq!(counter.projection.as_ref().unwrap().sequence_nr, 3);
    assert_eq!(counter.journal.sequence_nr, 4);
}

/// The comparison reads indexes, never a record's catalog row or snapshot:
/// from a cold cache it reads fewer pages than there are records, each with a
/// 32 KB catalog row and a 32 KB snapshot.
#[tokio::test]
async fn boot_comparison_reads_no_record_whole() {
    const RECORDS: usize = 64;
    let body = "x".repeat(32 * 1024);
    let url = sqlite_test_url("boot-comparison-reads");
    let store = TursoEventStore::new(&url, None).await.unwrap();
    for n in 0..RECORDS {
        let id = format!("doc-{n}");
        let pid = format!("acme:Doc:{id}");
        store
            .append(&pid, 0, &[test_envelope("Touch", serde_json::json!({}))])
            .await
            .unwrap();
        store.save_snapshot(&pid, 1, body.as_bytes()).await.unwrap();
        store
            .upsert_query_projection_with_state(
                "acme",
                "Doc",
                &id,
                "Open",
                &serde_json::json!({ "body": body }),
                &serde_json::json!({}),
                1,
            )
            .await
            .unwrap();
    }
    drop(store);
    // A copy at a new path: the engine shares one open database, and its
    // cache, per path within a process.
    let from = url.strip_prefix("file:").unwrap();
    let copy = sqlite_test_url("boot-comparison-reads-copy");
    let copy = copy.strip_prefix("file:").unwrap();
    for suffix in ["", "-wal"] {
        if std::path::Path::new(&format!("{from}{suffix}")).exists() {
            std::fs::copy(format!("{from}{suffix}"), format!("{copy}{suffix}")).unwrap();
        }
    }
    let io = super::read_count::CountingIo::new();
    let db = crate::driver::Database::local_with_io(copy, io.clone())
        .await
        .unwrap();
    let store = TursoEventStore::open(db, false, None).await.unwrap();

    io.reset();
    let lagging = store.lagging_query_projections("acme").await.unwrap();
    let reads = io.reads();
    assert!(lagging.is_empty(), "every record is current: {lagging:?}");
    assert!(
        reads > 0,
        "the comparison read nothing through the counted file"
    );
    assert!(
        reads < RECORDS,
        "the comparison read {reads} pages for {RECORDS} records: it read records whole"
    );
}
