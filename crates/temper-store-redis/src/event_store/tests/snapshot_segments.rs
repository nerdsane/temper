//! snapshot segments scenarios.

use super::*;

#[tokio::test]
async fn delayed_snapshot_rotation_preserves_the_durable_journal_tail() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();
    let events = (1..=10)
        .map(|sequence| test_envelope("OrderUpdated", serde_json::json!({ "sequence": sequence })))
        .collect::<Vec<_>>();
    assert_eq!(store.append(&pid, 0, &events).await.unwrap(), 10);

    store
        .save_snapshot(&pid, 5, b"{\"status\":\"halfway\"}")
        .await
        .unwrap();

    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&pid).unwrap();
    let pointer_key = RedisEventStore::current_segment_key(tenant, entity_type, entity_id);
    let pointer: String = store.client.get(&pointer_key).await.unwrap();
    assert_eq!(pointer, "1");
    let sealed: String = store
        .client
        .get(RedisEventStore::segment_key(
            tenant,
            entity_type,
            entity_id,
            0,
        ))
        .await
        .unwrap();
    let sealed: SegmentRecord = serde_json::from_str(&sealed).unwrap();
    assert_eq!(sealed.segment_index, 0);
    assert_eq!(sealed.start_sequence_nr, 1);
    assert_eq!(sealed.end_sequence_nr, Some(5));
    assert_eq!(sealed.snapshot_sequence, Some(5));
    assert_eq!(sealed.event_count, 5);
    assert!(sealed.sealed_at.is_some());

    let tail: String = store
        .client
        .get(RedisEventStore::segment_key(
            tenant,
            entity_type,
            entity_id,
            1,
        ))
        .await
        .unwrap();
    let tail: SegmentRecord = serde_json::from_str(&tail).unwrap();
    assert_eq!(tail.segment_index, 1);
    assert_eq!(tail.start_sequence_nr, 6);
    assert_eq!(tail.end_sequence_nr, Some(10));
    assert_eq!(tail.event_count, 5);
    assert!(tail.sealed_at.is_none());
    assert_eq!(sealed.created_at, tail.created_at);
    assert_eq!(store.read_events(&pid, 0).await.unwrap().len(), 10);
}

#[tokio::test]
async fn first_append_after_snapshot_only_state_builds_canonical_event_topology() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let snapshot = b"{\"status\":\"snapshot-only\"}".to_vec();
    let pid = unique_persistence_id();
    store.save_snapshot(&pid, 5, &snapshot).await.unwrap();
    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&pid).unwrap();
    let pointer_key = RedisEventStore::current_segment_key(tenant, entity_type, entity_id);
    let segment_zero_key = RedisEventStore::segment_key(tenant, entity_type, entity_id, 0);
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&pointer_key)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&segment_zero_key)
            .await
            .unwrap(),
        None
    );

    let sequence_nr = store
        .append_with_index_rows(
            &pid,
            0,
            &[test_envelope(
                "OrderUpdated",
                serde_json::json!({ "step": 1 }),
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
    assert_eq!(sequence_nr, 1);
    assert_eq!(
        store.client.get::<String, _>(&pointer_key).await.unwrap(),
        "0"
    );
    let canonical: String = store.client.get(&segment_zero_key).await.unwrap();
    let canonical: SegmentRecord = serde_json::from_str(&canonical).unwrap();
    assert_eq!(canonical.segment_index, 0);
    assert_eq!(canonical.start_sequence_nr, 1);
    assert_eq!(canonical.end_sequence_nr, Some(1));
    assert_eq!(canonical.snapshot_sequence, None);
    assert_eq!(canonical.event_count, 1);
    assert!(canonical.sealed_at.is_none());

    let legacy_pid = unique_persistence_id();
    let legacy_snapshot = b"{\"status\":\"legacy-snapshot-only\"}".to_vec();
    store
        .save_snapshot(&legacy_pid, 5, &legacy_snapshot)
        .await
        .unwrap();
    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&legacy_pid).unwrap();
    let pointer_key = RedisEventStore::current_segment_key(tenant, entity_type, entity_id);
    let segment_zero_key = RedisEventStore::segment_key(tenant, entity_type, entity_id, 0);
    let segment_one_key = RedisEventStore::segment_key(tenant, entity_type, entity_id, 1);
    let timestamp = sim_now().to_rfc3339();
    let legacy_zero = serde_json::json!({
        "segment_index": 0,
        "start_sequence_nr": 1,
        "end_sequence_nr": 5,
        "snapshot_sequence": 5,
        "event_count": 5,
        "sealed_at": timestamp,
        "created_at": timestamp,
    });
    let legacy_one = serde_json::json!({
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
            &segment_zero_key,
            legacy_zero.to_string(),
            None,
            None,
            false,
        )
        .await
        .unwrap();
    let _: () = store
        .client
        .set(&segment_one_key, legacy_one.to_string(), None, None, false)
        .await
        .unwrap();
    let _: () = store
        .client
        .set(&pointer_key, "1", None, None, false)
        .await
        .unwrap();

    let results = store
        .append_batch(&[PersistenceAppend {
            persistence_id: legacy_pid.clone(),
            expected_sequence: 0,
            events: vec![test_envelope(
                "OrderUpdated",
                serde_json::json!({ "step": 1 }),
            )],
            key_rows: Vec::new(),
            reconcile_keys: false,
            key_set_signature: None,
            snapshot_source: SnapshotSourceFence::Exact {
                sequence_nr: 5,
                state: legacy_snapshot,
            },
            batch_idempotency: None,
        }])
        .await
        .unwrap();
    assert_eq!(results[0].sequence_nr, 1);
    assert_eq!(
        store.client.get::<String, _>(&pointer_key).await.unwrap(),
        "0"
    );
    let canonical: String = store.client.get(&segment_zero_key).await.unwrap();
    let canonical: SegmentRecord = serde_json::from_str(&canonical).unwrap();
    assert_eq!(canonical.segment_index, 0);
    assert_eq!(canonical.start_sequence_nr, 1);
    assert_eq!(canonical.end_sequence_nr, Some(1));
    assert_eq!(canonical.snapshot_sequence, None);
    assert_eq!(canonical.event_count, 1);
    assert!(canonical.sealed_at.is_none());
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&segment_one_key)
            .await
            .unwrap(),
        None,
        "legacy snapshot-only active segment must be removed"
    );
    assert_eq!(store.read_events(&legacy_pid, 0).await.unwrap().len(), 1);
}

#[tokio::test]
async fn claimed_batch_retry_checks_content_before_stale_sequence() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();
    let mut append = PersistenceAppend {
        persistence_id: pid.clone(),
        expected_sequence: 0,
        events: vec![test_envelope(
            "CompositeEvent",
            serde_json::json!({"operation": "first"}),
        )],
        key_rows: Vec::new(),
        reconcile_keys: false,
        key_set_signature: None,
        snapshot_source: SnapshotSourceFence::Unchecked,
        batch_idempotency: Some(PersistenceBatchIdempotency {
            persistence_id: pid.clone(),
            idempotency_key: "stable-operation".to_string(),
            intent_hash: "intent-a".to_string(),
        }),
    };

    store
        .append_batch(std::slice::from_ref(&append))
        .await
        .expect("first claimed append");
    let retry = store
        .append_batch(std::slice::from_ref(&append))
        .await
        .expect("exact retry must reach the durable claim before stale sequence checks");
    assert!(retry[0].batch_already_applied);
    assert_eq!(retry[0].sequence_nr, 1);
    assert_eq!(store.read_events(&pid, 0).await.unwrap().len(), 1);

    append
        .batch_idempotency
        .as_mut()
        .expect("batch claim")
        .intent_hash = "intent-b".to_string();
    let error = store
        .append_batch(&[append])
        .await
        .expect_err("same claim key with different content must fail");
    assert!(error.to_string().contains("different intent"));
}
