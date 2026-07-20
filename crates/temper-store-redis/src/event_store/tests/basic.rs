//! Focused Redis event-store regression group.

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
    let read = store.read_events_with_head(&pid, 0).await.unwrap();
    assert_eq!(read.journal_head_sequence_nr, 2);
    let events = read.events;
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sequence_nr, 1);
    assert_eq!(events[1].sequence_nr, 2);
    assert_eq!(events[0].event_type, "OrderCreated");
    assert_eq!(events[1].event_type, "OrderApproved");

    // Partial read (from_sequence = 1 should skip event 1)
    let partial = store.read_events_with_head(&pid, 1).await.unwrap();
    assert_eq!(partial.journal_head_sequence_nr, 2);
    assert_eq!(partial.events.len(), 1);
    assert_eq!(partial.events[0].sequence_nr, 2);
    assert_eq!(partial.events[0].event_type, "OrderApproved");
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

    store
        .save_snapshot(&pid, 5, b"{\"status\":\"created\"}")
        .await
        .unwrap();

    let snapshot = store.load_snapshot(&pid).await.unwrap();
    assert_eq!(snapshot, Some((5, b"{\"status\":\"created\"}".to_vec())));

    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&pid).unwrap();
    let current_segment_key = RedisEventStore::current_segment_key(tenant, entity_type, entity_id);
    let segment_before: Option<String> = store.client.get(&current_segment_key).await.unwrap();
    let current_segment = segment_before
        .as_deref()
        .expect("current segment index after initial snapshot")
        .parse::<u64>()
        .unwrap();
    let segment_key = RedisEventStore::segment_key(tenant, entity_type, entity_id, current_segment);
    let next_segment_key =
        RedisEventStore::segment_key(tenant, entity_type, entity_id, current_segment + 1);
    let segment_record_before: Option<String> = store.client.get(&segment_key).await.unwrap();
    let next_segment_before: Option<String> = store.client.get(&next_segment_key).await.unwrap();
    store
        .replace_snapshot(
            &pid,
            5,
            b"{\"status\":\"created\"}",
            b"{\"status\":\"created-upgraded\"}",
        )
        .await
        .unwrap();
    assert_eq!(
        store.load_snapshot(&pid).await.unwrap(),
        Some((5, b"{\"status\":\"created-upgraded\"}".to_vec()))
    );

    let stale_replacement = store
        .replace_snapshot(
            &pid,
            5,
            b"{\"status\":\"created\"}",
            b"{\"status\":\"stale-overwrite\"}",
        )
        .await
        .expect_err("a stale same-boundary writer must lose");
    assert!(matches!(stale_replacement, PersistenceError::Storage(_)));
    assert_eq!(
        store.load_snapshot(&pid).await.unwrap(),
        Some((5, b"{\"status\":\"created-upgraded\"}".to_vec()))
    );
    let segment_after: Option<String> = store.client.get(&current_segment_key).await.unwrap();
    assert_eq!(
        segment_after, segment_before,
        "same-sequence snapshot replacement must not rotate event segments"
    );
    let segment_record_after: Option<String> = store.client.get(&segment_key).await.unwrap();
    let next_segment_after: Option<String> = store.client.get(&next_segment_key).await.unwrap();
    assert_eq!(segment_record_after, segment_record_before);
    assert_eq!(next_segment_after, next_segment_before);
    let history_key = RedisEventStore::snapshot_history_key(tenant, entity_type, entity_id, 5);
    let history: String = store.client.get(&history_key).await.unwrap();
    let history: SnapshotHistoryRecord = serde_json::from_str(&history).unwrap();
    assert_eq!(history.snapshot, b"{\"status\":\"created-upgraded\"}");

    // Overwrite
    store
        .save_snapshot(&pid, 8, b"{\"status\":\"shipped\"}")
        .await
        .unwrap();

    let updated = store.load_snapshot(&pid).await.unwrap();
    assert_eq!(updated, Some((8, b"{\"status\":\"shipped\"}".to_vec())));
}
