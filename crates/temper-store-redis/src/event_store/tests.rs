use super::*;
use temper_runtime::persistence::EventMetadata;

fn redis_url() -> Option<String> {
    std::env::var("REDIS_URL").ok()
}

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
            kernel: None,
        },
    }
}

fn unique_persistence_id() -> String {
    let id = uuid::Uuid::new_v4();
    format!("test-{id}:Order:ord-{id}")
}

async fn make_store() -> Option<RedisEventStore> {
    let url = redis_url()?;
    Some(
        RedisEventStore::new(&url)
            .await
            .expect("failed to connect to Redis"),
    )
}

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
    let events = store.read_events(&pid, 0).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].sequence_nr, 1);
    assert_eq!(events[1].sequence_nr, 2);
    assert_eq!(events[0].event_type, "OrderCreated");
    assert_eq!(events[1].event_type, "OrderApproved");

    let partial = store.read_events(&pid, 1).await.unwrap();
    assert_eq!(partial.len(), 1);
    assert_eq!(partial[0].sequence_nr, 2);
    assert_eq!(partial[0].event_type, "OrderApproved");
}

#[tokio::test]
async fn kernel_stream_metadata_roundtrips_with_historical_events() {
    use temper_runtime::persistence::{
        KernelEventMetadata, StreamDescriptorInputV1, StreamDescriptorV1, StreamEntityRef,
        StreamMutability, StreamStorageRefV1,
    };

    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();
    let historical = test_envelope("Created", serde_json::json!({}));
    let mut described = test_envelope("StreamUpdated", serde_json::json!({}));
    described.metadata.kernel = Some(KernelEventMetadata::V1 {
        stream_descriptor: StreamDescriptorV1::new(StreamDescriptorInputV1 {
            subject: StreamEntityRef::new("File", "file-1").unwrap(),
            authorization_parent: None,
            content_hash: "sha256:abc".into(),
            storage: StreamStorageRefV1::new("temper-fs/sha256:abc").unwrap(),
            byte_length: 3,
            content_type: None,
            content_event_sequence: 2,
            descriptor_event_sequence: 2,
            mutability: StreamMutability::Mutable,
        })
        .unwrap(),
    });
    store
        .append(&pid, 0, &[historical, described])
        .await
        .unwrap();
    let events = store.read_events(&pid, 0).await.unwrap();
    assert!(events[0].metadata.kernel.is_none());
    assert_eq!(
        events[1]
            .metadata
            .kernel
            .as_ref()
            .unwrap()
            .stream_descriptor()
            .descriptor_event_sequence(),
        2
    );
}

#[path = "tests/scoped_schema_pin_test.rs"]
mod scoped_schema_pin;
#[path = "tests/stream_publication.rs"]
mod stream_publication;

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
            0,
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
    assert_eq!(
        store.load_snapshot(&pid).await.unwrap(),
        Some((5, b"{\"status\":\"created\"}".to_vec()))
    );
    store
        .save_snapshot(&pid, 8, b"{\"status\":\"shipped\"}")
        .await
        .unwrap();
    assert_eq!(
        store.load_snapshot(&pid).await.unwrap(),
        Some((8, b"{\"status\":\"shipped\"}".to_vec()))
    );
}

#[tokio::test]
async fn list_entity_ids_returns_distinct_pairs() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let unique = uuid::Uuid::new_v4();
    let tenant_a = format!("tenant-a-{unique}");
    let tenant_b = format!("tenant-b-{unique}");
    for (persistence_id, event_type, id) in [
        (format!("{tenant_a}:Order:ord-1"), "OrderCreated", "ord-1"),
        (format!("{tenant_a}:Order:ord-2"), "OrderCreated", "ord-2"),
        (format!("{tenant_a}:Task:task-1"), "TaskCreated", "task-1"),
        (format!("{tenant_b}:Order:ord-9"), "OrderCreated", "ord-9"),
    ] {
        store
            .append(
                &persistence_id,
                0,
                &[test_envelope(event_type, serde_json::json!({ "id": id }))],
            )
            .await
            .unwrap();
    }
    assert_eq!(
        store.list_entity_ids(&tenant_a).await.unwrap(),
        vec![
            ("Order".to_string(), "ord-1".to_string()),
            ("Order".to_string(), "ord-2".to_string()),
            ("Task".to_string(), "task-1".to_string()),
        ]
    );
    assert_eq!(
        store.list_entity_ids(&tenant_b).await.unwrap(),
        vec![("Order".to_string(), "ord-9".to_string())]
    );
}

#[tokio::test]
async fn concurrent_appends_detect_conflict() {
    let Some(store) = make_store().await else {
        eprintln!("REDIS_URL not set, skipping test");
        return;
    };
    let pid = unique_persistence_id();
    let store1 = store.clone();
    let store2 = store.clone();
    let pid1 = pid.clone();
    let pid2 = pid.clone();
    let handle1 = tokio::spawn(async move {
        store1
            .append(
                &pid1,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "writer": 1 }),
                )],
            )
            .await
    });
    let handle2 = tokio::spawn(async move {
        store2
            .append(
                &pid2,
                0,
                &[test_envelope(
                    "OrderCreated",
                    serde_json::json!({ "writer": 2 }),
                )],
            )
            .await
    });
    let (r1, r2) = tokio::join!(handle1, handle2);
    let r1 = r1.unwrap();
    let r2 = r2.unwrap();
    let successes = [r1.is_ok(), r2.is_ok()].iter().filter(|&&ok| ok).count();
    let conflicts = [&r1, &r2]
        .iter()
        .filter(|result| matches!(result, Err(PersistenceError::ConcurrencyViolation { .. })))
        .count();
    assert_eq!(successes, 1, "exactly one writer should succeed");
    assert_eq!(conflicts, 1, "exactly one writer should see a conflict");
}
