use super::*;
use temper_runtime::persistence::EventMetadata;

fn test_envelope(event_type: &str, payload: serde_json::Value) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload,
        metadata: EventMetadata {
            event_id: uuid::Uuid::new_v4(),
            causation_id: uuid::Uuid::new_v4(),
            correlation_id: uuid::Uuid::new_v4(),
            timestamp: chrono::Utc::now(),
            actor_id: "redis-test".to_string(),
        },
    }
}

fn unique_persistence_id() -> String {
    let id = uuid::Uuid::new_v4();
    format!("test-{id}:Order:ord-{id}")
}

async fn make_store() -> RedisEventStore {
    let url = std::env::var("REDIS_URL").expect("REDIS_URL for ignored Redis integration test");
    RedisEventStore::new(&url)
        .await
        .expect("failed to connect to Redis")
}

#[tokio::test]
#[ignore = "requires REDIS_URL and a live Redis service"]
async fn snapshot_and_journal_tail_recover_without_segment_metadata() {
    let store = make_store().await;
    let pid = unique_persistence_id();
    let new_seq = store
        .append(
            &pid,
            0,
            &[test_envelope(
                "OrderCreated",
                serde_json::json!({ "id": "ord" }),
            )],
        )
        .await
        .expect("append first event");
    assert_eq!(new_seq, 1);

    store
        .save_snapshot(&pid, 1, b"snapshot-at-one")
        .await
        .expect("save observable snapshot boundary");

    let next_seq = store
        .append(
            &pid,
            1,
            &[test_envelope(
                "OrderApproved",
                serde_json::json!({ "ok": true }),
            )],
        )
        .await
        .expect("second append chains from the advanced sequence");
    assert_eq!(next_seq, 2);

    assert_eq!(
        store.load_snapshot(&pid).await.unwrap(),
        Some((1, b"snapshot-at-one".to_vec()))
    );
    let tail = store.read_events(&pid, 1).await.unwrap();
    assert_eq!(tail.len(), 1);
    assert_eq!(tail[0].sequence_nr, 2);

    let latest = store
        .read_latest_events(std::slice::from_ref(&pid))
        .await
        .unwrap();
    assert_eq!(latest[0].as_ref().unwrap().sequence_nr, 2);
}

#[tokio::test]
#[ignore = "requires REDIS_URL and a live Redis service"]
async fn guarded_append_rejects_stale_context_without_writing_target() {
    let store = make_store().await;
    let context_id = unique_persistence_id();
    let target_id = unique_persistence_id();
    store
        .append(
            &context_id,
            0,
            &[test_envelope("Created", serde_json::json!({}))],
        )
        .await
        .unwrap();
    let error = store
        .append_batch_guarded(
            &[PersistenceAppend {
                persistence_id: target_id.clone(),
                expected_sequence: 0,
                events: vec![test_envelope("FieldsPatched", serde_json::json!({}))],
                key_rows: None,
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            }],
            &[PersistenceSequenceGuard {
                persistence_id: context_id.clone(),
                expected_sequence: 0,
            }],
        )
        .await
        .expect_err("stale Redis guard must abort target append");
    assert!(matches!(error, PersistenceError::PreconditionFailed { .. }));
    assert!(store.read_events(&target_id, 0).await.unwrap().is_empty());
    let result = store
        .append_batch_guarded(
            &[PersistenceAppend {
                persistence_id: target_id.clone(),
                expected_sequence: 0,
                events: vec![test_envelope("FieldsPatched", serde_json::json!({}))],
                key_rows: None,
                vector_rows: Vec::new(),
                reconcile_vectors: false,
            }],
            &[PersistenceSequenceGuard {
                persistence_id: context_id,
                expected_sequence: 1,
            }],
        )
        .await
        .expect("current Redis guard should commit target atomically");
    assert_eq!(result[0].sequence_nr, 1);
    assert_eq!(store.read_events(&target_id, 0).await.unwrap().len(), 1);
}
