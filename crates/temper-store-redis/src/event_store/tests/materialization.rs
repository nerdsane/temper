//! materialization scenarios.

use super::*;

#[tokio::test]
async fn materialization_handoff_retires_snapshot_and_blocks_delayed_recreation() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();
    let migration_snapshot = b"{\"status\":\"migration\",\"counter\":10}".to_vec();
    store
        .save_snapshot(&pid, 5, &migration_snapshot)
        .await
        .unwrap();

    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&pid).unwrap();
    assert!(
        store
            .list_entity_ids(tenant)
            .await
            .unwrap()
            .contains(&(entity_type.to_string(), entity_id.to_string())),
        "a snapshot-only stream must be discoverable before its first journal append"
    );
    assert!(
        store
            .list_entity_ids_by_type(tenant, entity_type)
            .await
            .unwrap()
            .contains(&entity_id.to_string()),
        "type-scoped discovery must include a snapshot-only stream"
    );
    let snapshot_key = RedisEventStore::snapshot_key(tenant, entity_type, entity_id);
    let history_key = RedisEventStore::snapshot_history_key(tenant, entity_type, entity_id, 5);
    let history_before: String = store.client.get(&history_key).await.unwrap();

    let new_sequence = store
        .append_with_index_rows(
            &pid,
            0,
            &[
                test_envelope(
                    STATE_MATERIALIZATION_EVENT_TYPE,
                    serde_json::json!({
                        "schema": "temper.state-materialization.v1",
                        "state": {
                            "entity_type": entity_type,
                            "entity_id": entity_id,
                            "status": "Ready",
                            "item_count": 0,
                            "counters": {"counter": 10},
                            "booleans": {},
                            "lists": {},
                            "fields": {"Id": entity_id, "Status": "Ready"},
                            "events": [],
                            "total_event_count": 0,
                            "events_since_snapshot": 0,
                            "last_snapshot_sequence_nr": 0,
                            "sequence_nr": 0,
                            "processed_idempotency_keys": {},
                        },
                    }),
                ),
                test_envelope("OrderIncremented", serde_json::json!({"amount": 1})),
            ],
            &[],
            &[],
            IndexReconciliation {
                snapshot_source: SnapshotSourceFence::Exact {
                    sequence_nr: 5,
                    state: migration_snapshot,
                },
                ..IndexReconciliation::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(new_sequence, 2);
    assert_eq!(store.load_snapshot(&pid).await.unwrap(), None);
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&snapshot_key)
            .await
            .unwrap(),
        None,
        "the materialized journal atomically retires its migration snapshot"
    );

    store
        .save_snapshot(&pid, 5, b"{\"status\":\"delayed\",\"counter\":10}")
        .await
        .unwrap();

    assert_eq!(
        store.load_snapshot(&pid).await.unwrap(),
        None,
        "an ahead-of-journal delayed writer cannot recreate a retired migration snapshot"
    );
    assert_eq!(
        store.client.get::<String, _>(&history_key).await.unwrap(),
        history_before,
        "the delayed no-op must not replace snapshot history"
    );
    let events = store.read_events(&pid, 0).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].event_type, STATE_MATERIALIZATION_EVENT_TYPE);

    store
        .append(
            &pid,
            2,
            &[
                test_envelope("OrderIncremented", serde_json::json!({"amount": 1})),
                test_envelope("OrderIncremented", serde_json::json!({"amount": 1})),
                test_envelope("OrderIncremented", serde_json::json!({"amount": 1})),
                test_envelope("OrderIncremented", serde_json::json!({"amount": 1})),
            ],
        )
        .await
        .unwrap();
    let pointer_key = RedisEventStore::current_segment_key(tenant, entity_type, entity_id);
    let pointer_before: Option<String> = store.client.get(&pointer_key).await.unwrap();
    let active_segment_key = RedisEventStore::segment_key(
        tenant,
        entity_type,
        entity_id,
        pointer_before.as_deref().unwrap_or("0").parse().unwrap(),
    );
    let active_segment_before: Option<String> =
        store.client.get(&active_segment_key).await.unwrap();

    store
        .save_snapshot(
            &pid,
            5,
            b"{\"status\":\"delayed-after-catch-up\",\"counter\":10}",
        )
        .await
        .unwrap();

    assert_eq!(
        store.load_snapshot(&pid).await.unwrap(),
        None,
        "an unchecked writer stays retired after the journal catches and passes its sequence"
    );
    assert_eq!(
        store.client.get::<String, _>(&history_key).await.unwrap(),
        history_before
    );
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&pointer_key)
            .await
            .unwrap(),
        pointer_before,
        "the rejected write must not rotate segment topology"
    );
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&active_segment_key)
            .await
            .unwrap(),
        active_segment_before,
        "the rejected write must not rewrite active segment metadata"
    );
    let events = store.read_events(&pid, 0).await.unwrap();
    assert_eq!(events.len(), 6);
    assert_eq!(events.last().unwrap().sequence_nr, 6);
}

#[tokio::test]
async fn snapshot_ahead_of_journal_does_not_rotate_and_append_repairs_legacy_topology() {
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
                test_envelope("OrderCreated", serde_json::json!({"step": 1})),
                test_envelope("OrderUpdated", serde_json::json!({"step": 2})),
            ],
        )
        .await
        .unwrap();
    let snapshot = b"{\"status\":\"migration-baseline\"}".to_vec();
    store.save_snapshot(&pid, 5, &snapshot).await.unwrap();

    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&pid).unwrap();
    let pointer_key = RedisEventStore::current_segment_key(tenant, entity_type, entity_id);
    let segment_zero_key = RedisEventStore::segment_key(tenant, entity_type, entity_id, 0);
    let segment_one_key = RedisEventStore::segment_key(tenant, entity_type, entity_id, 1);
    assert_eq!(
        store.client.get::<String, _>(&pointer_key).await.unwrap(),
        "0",
        "a snapshot ahead of journal HWM 2 must not rotate to segment 1"
    );
    let canonical: String = store.client.get(&segment_zero_key).await.unwrap();
    let canonical: SegmentRecord = serde_json::from_str(&canonical).unwrap();
    assert_eq!(canonical.end_sequence_nr, Some(2));
    assert_eq!(canonical.snapshot_sequence, None);
    assert!(canonical.sealed_at.is_none());

    let timestamp = sim_now().to_rfc3339();
    let legacy_ghost = serde_json::json!({
        "segment_index": 1,
        "start_sequence_nr": 6,
        "end_sequence_nr": null,
        "snapshot_sequence": null,
        "event_count": 0,
        "sealed_at": null,
        "created_at": timestamp,
    });
    let _: () = store
        .client
        .set(
            &segment_one_key,
            legacy_ghost.to_string(),
            None,
            None,
            false,
        )
        .await
        .unwrap();
    let _: () = store
        .client
        .set(&pointer_key, "1", None, None, false)
        .await
        .unwrap();

    store
        .append_with_index_rows(
            &pid,
            2,
            &[test_envelope(
                "OrderUpdated",
                serde_json::json!({"step": 3}),
            )],
            &[],
            &[],
            IndexReconciliation {
                snapshot_source: SnapshotSourceFence::Exact {
                    sequence_nr: 5,
                    state: snapshot,
                },
                ..IndexReconciliation::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(
        store.client.get::<String, _>(&pointer_key).await.unwrap(),
        "0"
    );
    let repaired: String = store.client.get(&segment_zero_key).await.unwrap();
    let repaired: SegmentRecord = serde_json::from_str(&repaired).unwrap();
    assert_eq!(repaired.start_sequence_nr, 1);
    assert_eq!(repaired.end_sequence_nr, Some(3));
    assert_eq!(repaired.event_count, 3);
    assert_eq!(repaired.snapshot_sequence, None);
    assert!(repaired.sealed_at.is_none());
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&segment_one_key)
            .await
            .unwrap(),
        None,
        "append must remove the legacy future-start segment"
    );
}
