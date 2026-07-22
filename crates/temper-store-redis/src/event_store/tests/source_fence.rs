//! source fence scenarios.

use super::*;

#[tokio::test]
async fn checked_snapshot_save_rejects_a_changed_source_without_mutation() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();
    let first = b"{\"status\":\"first\"}".to_vec();
    let replacement = b"{\"status\":\"replacement\"}".to_vec();
    let checked = b"{\"status\":\"checked\"}".to_vec();
    store.save_snapshot(&pid, 5, &first).await.unwrap();
    store.save_snapshot(&pid, 5, &replacement).await.unwrap();

    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&pid).unwrap();
    let pointer_key = RedisEventStore::current_segment_key(tenant, entity_type, entity_id);
    let segment_zero_key = RedisEventStore::segment_key(tenant, entity_type, entity_id, 0);
    let segment_one_key = RedisEventStore::segment_key(tenant, entity_type, entity_id, 1);
    let history_key = RedisEventStore::snapshot_history_key(tenant, entity_type, entity_id, 5);
    let pointer_before: Option<String> = store.client.get(&pointer_key).await.unwrap();
    let segment_zero_before: Option<String> = store.client.get(&segment_zero_key).await.unwrap();
    let segment_one_before: Option<String> = store.client.get(&segment_one_key).await.unwrap();
    let history_before: String = store.client.get(&history_key).await.unwrap();

    let stale = store
        .save_snapshot_if_source(
            &pid,
            5,
            &checked,
            &SnapshotSourceFence::Exact {
                sequence_nr: 5,
                state: first,
            },
            None,
        )
        .await
        .unwrap_err();
    assert!(matches!(stale, PersistenceError::SnapshotGenerationChanged));
    let absent = store
        .save_snapshot_if_source(&pid, 5, &checked, &SnapshotSourceFence::Absent, None)
        .await
        .unwrap_err();
    assert!(matches!(
        absent,
        PersistenceError::SnapshotGenerationChanged
    ));
    assert_eq!(
        store.load_snapshot(&pid).await.unwrap(),
        Some((5, replacement.clone()))
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
        pointer_before
    );
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&segment_zero_key)
            .await
            .unwrap(),
        segment_zero_before
    );
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&segment_one_key)
            .await
            .unwrap(),
        segment_one_before
    );

    store
        .save_snapshot_if_source(
            &pid,
            5,
            &checked,
            &SnapshotSourceFence::Exact {
                sequence_nr: 5,
                state: replacement,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        store.load_snapshot(&pid).await.unwrap(),
        Some((5, checked.clone()))
    );
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&pointer_key)
            .await
            .unwrap(),
        pointer_before,
        "an accepted same-sequence CAS must not rotate"
    );

    store
        .save_snapshot_if_source(
            &pid,
            8,
            b"{\"status\":\"newer\"}",
            &SnapshotSourceFence::Exact {
                sequence_nr: 5,
                state: checked,
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .client
            .get::<Option<String>, _>(&pointer_key)
            .await
            .unwrap(),
        None,
        "an accepted newer CAS without a journal must not rotate"
    );

    let absent_pid = unique_persistence_id();
    store
        .save_snapshot_if_source(
            &absent_pid,
            1,
            b"{\"status\":\"initial\"}",
            &SnapshotSourceFence::Absent,
            None,
        )
        .await
        .unwrap();
    assert_eq!(
        store.load_snapshot(&absent_pid).await.unwrap(),
        Some((1, b"{\"status\":\"initial\"}".to_vec()))
    );
}

#[tokio::test]
async fn append_rejects_a_changed_snapshot_source_before_mutating_the_journal() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();
    assert_eq!(
        store
            .append(
                &pid,
                0,
                &[test_envelope("OrderCreated", serde_json::json!({}))]
            )
            .await
            .unwrap(),
        1
    );
    let first_snapshot = b"{\"status\":\"created\"}".to_vec();
    let replacement_snapshot = b"{\"status\":\"replaced\"}".to_vec();
    store.save_snapshot(&pid, 1, &first_snapshot).await.unwrap();
    store
        .save_snapshot(&pid, 1, &replacement_snapshot)
        .await
        .unwrap();

    let (tenant, entity_type, entity_id) = parse_persistence_id_parts(&pid).unwrap();
    let pointer_key = RedisEventStore::current_segment_key(tenant, entity_type, entity_id);
    let pointer_before: String = store.client.get(&pointer_key).await.unwrap();
    let active_key = RedisEventStore::segment_key(
        tenant,
        entity_type,
        entity_id,
        pointer_before.parse().unwrap(),
    );
    let active_before: String = store.client.get(&active_key).await.unwrap();
    let next_event = test_envelope("OrderUpdated", serde_json::json!({ "step": 2 }));

    let stale = store
        .append_with_index_rows(
            &pid,
            1,
            std::slice::from_ref(&next_event),
            &[],
            &[],
            IndexReconciliation {
                snapshot_source: SnapshotSourceFence::Exact {
                    sequence_nr: 1,
                    state: first_snapshot,
                },
                ..IndexReconciliation::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(stale, PersistenceError::SnapshotGenerationChanged));
    assert_eq!(store.read_events(&pid, 0).await.unwrap().len(), 1);
    assert_eq!(
        store.client.get::<String, _>(&pointer_key).await.unwrap(),
        pointer_before
    );
    assert_eq!(
        store.client.get::<String, _>(&active_key).await.unwrap(),
        active_before
    );

    let absent = store
        .append_with_index_rows(
            &pid,
            1,
            std::slice::from_ref(&next_event),
            &[],
            &[],
            IndexReconciliation {
                snapshot_source: SnapshotSourceFence::Absent,
                ..IndexReconciliation::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        absent,
        PersistenceError::SnapshotGenerationChanged
    ));
    assert_eq!(store.read_events(&pid, 0).await.unwrap().len(), 1);

    let new_sequence = store
        .append_with_index_rows(
            &pid,
            1,
            &[next_event],
            &[],
            &[],
            IndexReconciliation {
                snapshot_source: SnapshotSourceFence::Exact {
                    sequence_nr: 1,
                    state: replacement_snapshot,
                },
                ..IndexReconciliation::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(new_sequence, 2);
    assert_eq!(store.read_events(&pid, 0).await.unwrap().len(), 2);
}
