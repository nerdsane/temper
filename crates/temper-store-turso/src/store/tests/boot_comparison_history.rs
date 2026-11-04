//! A restart's projection comparison costs what could be behind, not the
//! tenant's history, and still finds every projection that is behind.
use super::*;
use temper_runtime::persistence::{CommitRows, ProjectionRow};

const TENANT: &str = "acme";

/// Append `events` events to `pid` from `from`, committing the record's
/// catalog row with them, as every move does.
async fn commit_projected(store: &TursoEventStore, pid: &str, from: u64, events: usize) {
    let fields = serde_json::json!({ "title": "current" });
    let events: Vec<_> = (0..events)
        .map(|_| test_envelope("Touch", serde_json::json!({})))
        .collect();
    store
        .append_with_rows(
            pid,
            from,
            &events,
            CommitRows {
                projection: Some(ProjectionRow::Upsert {
                    status: "Open",
                    fields: &fields,
                    state: &serde_json::json!({}),
                }),
                ..CommitRows::default()
            },
        )
        .await
        .unwrap();
}

/// Pages a cold restart's comparison reads for `records` records of `events`
/// events each, every one committed with its catalog row.
async fn current_tenant_comparison_reads(name: &str, records: usize, events: usize) -> usize {
    let url = sqlite_test_url(name);
    let store = TursoEventStore::new(&url, None).await.unwrap();
    for n in 0..records {
        commit_projected(&store, &format!("{TENANT}:Doc:doc-{n}"), 0, events).await;
    }
    drop(store);
    let (store, io) = super::read_count::reopen_cold(&url, &format!("{name}-copy")).await;
    io.reset();
    let lagging = store.lagging_query_projections(TENANT).await.unwrap();
    let reads = io.reads();
    assert!(lagging.is_empty(), "every record is current: {lagging:?}");
    reads
}

/// A busy record gains an event every few seconds, and a restart must not pay
/// for each of them again. With every projection committed with its events,
/// records 400 events long cost the comparison no more reads than records one
/// event long.
#[tokio::test]
async fn boot_comparison_reads_do_not_grow_with_history() {
    const RECORDS: usize = 50;
    let short = current_tenant_comparison_reads("boot-history-short", RECORDS, 1).await;
    let long = current_tenant_comparison_reads("boot-history-long", RECORDS, 400).await;
    assert!(
        long <= short,
        "the comparison read {long} pages for {RECORDS} current records of 400 events \
         each, against {short} for one event each: it reads the tenant's history"
    );
}

/// A database written before the projection delta existed, with one record's
/// events committed without its catalog row and another's row left behind its
/// journal: its first start finds both, and only both.
#[tokio::test]
async fn boot_comparison_finds_rows_behind_from_before_the_delta() {
    let url = sqlite_test_url("boot-before-delta");
    let store = TursoEventStore::new(&url, None).await.unwrap();
    for id in ["current", "stale"] {
        commit_projected(&store, &format!("{TENANT}:Doc:{id}"), 0, 3).await;
    }
    store
        .append(
            &format!("{TENANT}:Doc:unprojected"),
            0,
            &[test_envelope("Touch", serde_json::json!({}))],
        )
        .await
        .unwrap();
    let conn = store
        .write_connection("test", super::super::WritePriority::High)
        .await
        .unwrap();
    conn.execute(
        "UPDATE entity_catalog SET sequence_nr = 1
         WHERE tenant = ?1 AND entity_type = 'Doc' AND entity_id = ?2",
        params![TENANT, "stale"],
    )
    .await
    .unwrap();
    conn.execute("DROP TABLE entity_projection_delta", ())
        .await
        .unwrap();
    drop(conn);
    drop(store);

    let store = TursoEventStore::new(&url, None).await.unwrap();
    let lagging = store.lagging_query_projections(TENANT).await.unwrap();
    let observed: Vec<_> = lagging
        .iter()
        .map(|g| {
            (
                g.journal.entity_id.as_str(),
                g.projection.as_ref().map(|p| p.sequence_nr),
                g.journal.sequence_nr,
            )
        })
        .collect();
    assert_eq!(
        observed,
        vec![("stale", Some(1), 3), ("unprojected", None, 1)]
    );
}
