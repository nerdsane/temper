//! append scenarios.

use super::*;

#[tokio::test]
async fn append_and_read_events_roundtrip() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();

    let new_seq = store
        .append(
            &pid,
            0,
            &[
                test_envelope("OrderCreated", serde_json::json!({ "id": "ord-1" })),
                test_envelope("OrderApproved", serde_json::json!({ "approved": true })),
            ],
        )
        .await
        .unwrap();

    assert_eq!(new_seq, 2);

    // Read all events
    let events = store.read_events(&pid, 0).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sequence_nr, 1);
    assert_eq!(events[1].sequence_nr, 2);
    assert_eq!(events[0].event_type, "OrderCreated");
    assert_eq!(events[1].event_type, "OrderApproved");

    // Partial read (from_sequence = 1 should skip event 1)
    let partial = store.read_events(&pid, 1).await.unwrap();
    assert_eq!(partial.len(), 1);
    assert_eq!(partial[0].sequence_nr, 2);
    assert_eq!(partial[0].event_type, "OrderApproved");
}

#[tokio::test]
async fn legacy_terminal_boundary_is_reconstructed_across_bounded_pages() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();
    let terminal_sequence = JOURNAL_BOUNDARY_PAGE_SIZE as u64 + 1;
    let latest_sequence = terminal_sequence + 1;
    let events = (1..=latest_sequence)
        .map(|sequence| {
            if sequence == terminal_sequence {
                test_envelope(
                    "Delete",
                    serde_json::json!({
                        "action": "Delete",
                        "from_status": "Ready",
                        "to_status": "Deleted",
                    }),
                )
            } else if sequence == latest_sequence {
                test_envelope(
                    "LegacySuffix",
                    serde_json::json!({
                        "action": "LegacySuffix",
                        "from_status": "Deleted",
                        "to_status": "Ready",
                    }),
                )
            } else {
                test_envelope("OrderUpdated", serde_json::json!({ "sequence": sequence }))
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        store.append(&pid, 0, &events).await.unwrap(),
        latest_sequence
    );

    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&pid).unwrap();
    let terminal_key = RedisEventStore::terminal_sequence_key(tenant, entity_type, entity_id);
    let _: i64 = store.client.del(&terminal_key).await.unwrap();

    let boundary = store.journal_boundary(&pid).await.unwrap();
    assert_eq!(boundary.latest_sequence, latest_sequence);
    assert_eq!(boundary.first_terminal_sequence, Some(terminal_sequence));
    assert_eq!(
        store.client.get::<String, _>(&terminal_key).await.unwrap(),
        terminal_sequence.to_string(),
        "legacy reconstruction must persist the first terminal boundary"
    );
    assert_eq!(
        store.journal_boundary(&pid).await.unwrap(),
        boundary,
        "later boundary reads must use the installed O(1) metadata"
    );
}

#[tokio::test]
async fn appending_to_a_legacy_stream_preserves_terminal_boundary_migration() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();
    store
        .append(
            &pid,
            0,
            &[
                test_envelope("Create", serde_json::json!({ "to_status": "Ready" })),
                test_envelope(
                    "Delete",
                    serde_json::json!({
                        "action": "Delete",
                        "from_status": "Ready",
                        "to_status": "Deleted",
                    }),
                ),
                test_envelope(
                    "LegacySuffix",
                    serde_json::json!({
                        "from_status": "Deleted",
                        "to_status": "Ready",
                    }),
                ),
            ],
        )
        .await
        .expect("seed stream with a terminal transition");

    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&pid).unwrap();
    let terminal_key = RedisEventStore::terminal_sequence_key(tenant, entity_type, entity_id);
    let _: i64 = store.client.del(&terminal_key).await.unwrap();

    store
        .append(
            &pid,
            3,
            &[test_envelope(
                "AppendedBeforeMigration",
                serde_json::json!({ "to_status": "Ready" }),
            )],
        )
        .await
        .expect("append to legacy stream before its first boundary read");

    assert_eq!(
        store
            .journal_boundary(&pid)
            .await
            .expect("migrate legacy terminal metadata")
            .first_terminal_sequence,
        Some(2),
        "append must leave missing legacy metadata absent so bounded migration can scan the journal"
    );
}

#[tokio::test]
async fn append_with_wrong_sequence_fails() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();

    store
        .append(
            &pid,
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord-1" }),
            )],
        )
        .await
        .unwrap();

    let err = store
        .append(
            &pid,
            0, // stale: actual is 1
            &[test_envelope(
                "OrderUpdated",
                serde_json::json!({ "step": 2 }),
            )],
        )
        .await
        .unwrap_err();

    assert!(matches!(
        err,
        PersistenceError::ConcurrencyViolation {
            expected: 0,
            actual: 1
        }
    ));
}

#[tokio::test]
async fn snapshot_save_and_load_roundtrip() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();
    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&pid).unwrap();
    let snapshot_key = RedisEventStore::snapshot_key(tenant, entity_type, entity_id);
    let pointer_key = RedisEventStore::current_segment_key(tenant, entity_type, entity_id);
    let segment_zero_key = RedisEventStore::segment_key(tenant, entity_type, entity_id, 0);
    let segment_one_key = RedisEventStore::segment_key(tenant, entity_type, entity_id, 1);
    let history_five_key = RedisEventStore::snapshot_history_key(tenant, entity_type, entity_id, 5);

    store
        .save_snapshot(&pid, 5, b"{\"status\":\"created\"}")
        .await
        .unwrap();

    let snapshot = store.load_snapshot(&pid).await.unwrap();
    assert_eq!(snapshot, Some((5, b"{\"status\":\"created\"}".to_vec())));

    let raw_snapshot: String = store.client.get(&snapshot_key).await.unwrap();
    let raw_history: String = store.client.get(&history_five_key).await.unwrap();
    let pointer: Option<String> = store.client.get(&pointer_key).await.unwrap();
    let segment_zero: Option<String> = store.client.get(&segment_zero_key).await.unwrap();
    let segment_one: Option<String> = store.client.get(&segment_one_key).await.unwrap();
    assert_eq!(pointer, None, "snapshot-only state has no event segment");
    assert_eq!(segment_zero, None);
    assert_eq!(segment_one, None);

    store
        .save_snapshot(&pid, 5, b"{\"status\":\"created\"}")
        .await
        .unwrap();
    assert_eq!(
        store.client.get::<String, _>(&snapshot_key).await.unwrap(),
        raw_snapshot,
        "identical snapshot writes must not replace the current row"
    );
    assert_eq!(
        store
            .client
            .get::<String, _>(&history_five_key)
            .await
            .unwrap(),
        raw_history,
        "identical snapshot writes must not churn history"
    );
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&pointer_key)
            .await
            .unwrap(),
        pointer
    );
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&segment_zero_key)
            .await
            .unwrap(),
        segment_zero
    );
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&segment_one_key)
            .await
            .unwrap(),
        segment_one
    );

    store
        .save_snapshot(&pid, 5, b"{\"status\":\"approved\"}")
        .await
        .unwrap();
    assert_eq!(
        store.load_snapshot(&pid).await.unwrap(),
        Some((5, b"{\"status\":\"approved\"}".to_vec()))
    );
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&pointer_key)
            .await
            .unwrap(),
        pointer,
        "same-sequence byte replacement must not advance the segment pointer"
    );
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&segment_zero_key)
            .await
            .unwrap(),
        segment_zero
    );
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&segment_one_key)
            .await
            .unwrap(),
        segment_one
    );

    store
        .save_snapshot(&pid, 4, b"{\"status\":\"delayed\"}")
        .await
        .unwrap();
    assert_eq!(
        store.load_snapshot(&pid).await.unwrap(),
        Some((5, b"{\"status\":\"approved\"}".to_vec())),
        "an older writer must not regress the current snapshot"
    );
    let history_four_key = RedisEventStore::snapshot_history_key(tenant, entity_type, entity_id, 4);
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&history_four_key)
            .await
            .unwrap(),
        None,
        "an ignored older write must not create history"
    );
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&pointer_key)
            .await
            .unwrap(),
        pointer
    );

    store
        .save_snapshot(&pid, 8, b"{\"status\":\"shipped\"}")
        .await
        .unwrap();

    let updated = store.load_snapshot(&pid).await.unwrap();
    assert_eq!(updated, Some((8, b"{\"status\":\"shipped\"}".to_vec())));
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&pointer_key)
            .await
            .unwrap(),
        None,
        "a newer snapshot without a journal must not create event segments"
    );
}
