use super::*;
use temper_runtime::persistence::{COMPOSITE_EVENT_TYPE, EventMetadata};

fn test_envelope(event_type: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload: serde_json::json!({"test": true}),
        metadata: EventMetadata {
            event_id: uuid::Uuid::nil(),
            causation_id: uuid::Uuid::nil(),
            correlation_id: uuid::Uuid::nil(),
            timestamp: chrono::DateTime::UNIX_EPOCH,
            actor_id: "key-test".to_string(),
        },
    }
}

#[tokio::test]
async fn tombstone_atomically_retires_declared_keys_before_recreation() {
    let store = SimEventStore::no_faults(42);
    let persistence_id = "default:Order:keyed-generation";
    let original = temper_runtime::persistence::EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "hash-original".to_string(),
    };
    store
        .append_with_keys(
            persistence_id,
            0,
            &[test_envelope("Created")],
            std::slice::from_ref(&original),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .lookup_by_key("default", "Order", "path", "hash-original")
            .await
            .unwrap(),
        Some("keyed-generation".to_string())
    );

    store
        .append_with_keys(
            persistence_id,
            1,
            &[
                test_envelope("Deleted"),
                test_envelope(COMPOSITE_EVENT_TYPE),
            ],
            std::slice::from_ref(&original),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .lookup_by_key("default", "Order", "path", "hash-original")
            .await
            .unwrap(),
        None,
        "trailing audit records must not hide a terminal tombstone"
    );

    let recreated = temper_runtime::persistence::EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "hash-recreated".to_string(),
    };
    store
        .append_with_keys(
            persistence_id,
            3,
            &[test_envelope("Created")],
            std::slice::from_ref(&recreated),
        )
        .await
        .unwrap();
    assert_eq!(
        store
            .lookup_by_key("default", "Order", "path", "hash-recreated")
            .await
            .unwrap(),
        Some("keyed-generation".to_string())
    );

    store
        .append_batch(&[PersistenceAppend {
            persistence_id: persistence_id.to_string(),
            expected_sequence: 4,
            events: vec![
                test_envelope("Deleted"),
                test_envelope(COMPOSITE_EVENT_TYPE),
            ],
            key_rows: None,
        }])
        .await
        .unwrap();
    assert_eq!(
        store
            .lookup_by_key("default", "Order", "path", "hash-recreated")
            .await
            .unwrap(),
        None,
        "atomic composite/batch deletion must retire declared keys too"
    );
}

#[tokio::test]
async fn raw_append_preserves_declared_keys_it_cannot_recompute() {
    let store = SimEventStore::no_faults(42);
    let persistence_id = "default:Order:raw-key-preservation";
    let key = temper_runtime::persistence::EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "hash-preserved".to_string(),
    };
    store
        .append_with_keys(
            persistence_id,
            0,
            &[test_envelope("Created")],
            std::slice::from_ref(&key),
        )
        .await
        .unwrap();
    store
        .append(persistence_id, 1, &[test_envelope("ExternalAudit")])
        .await
        .unwrap();

    assert_eq!(
        store
            .lookup_by_key("default", "Order", "path", "hash-preserved")
            .await
            .unwrap(),
        Some("raw-key-preservation".to_string())
    );
}

#[tokio::test]
async fn batch_key_intent_distinguishes_preserve_from_authoritative_clear() {
    let store = SimEventStore::no_faults(42);
    let persistence_id = "default:Order:batch-key-intent";
    let key = temper_runtime::persistence::EntityKeyRow {
        key_name: "path".to_string(),
        key_hash: "hash-batch-key-intent".to_string(),
    };
    store
        .append_with_keys(
            persistence_id,
            0,
            &[test_envelope("Created")],
            std::slice::from_ref(&key),
        )
        .await
        .unwrap();

    store
        .append_batch(&[PersistenceAppend {
            persistence_id: persistence_id.to_string(),
            expected_sequence: 1,
            events: vec![test_envelope("ExternalAudit")],
            key_rows: None,
        }])
        .await
        .unwrap();
    assert_eq!(
        store
            .lookup_by_key("default", "Order", "path", &key.key_hash)
            .await
            .unwrap(),
        Some("batch-key-intent".to_string()),
        "raw batch append must preserve claims it cannot recompute"
    );

    store
        .append_batch(&[PersistenceAppend {
            persistence_id: persistence_id.to_string(),
            expected_sequence: 2,
            events: vec![test_envelope("Deleted"), test_envelope("Created")],
            key_rows: None,
        }])
        .await
        .unwrap();
    assert_eq!(
        store
            .lookup_by_key("default", "Order", "path", &key.key_hash)
            .await
            .unwrap(),
        None,
        "a raw batch crossing a generation boundary must not preserve old claims"
    );

    store
        .append_with_keys(
            persistence_id,
            4,
            &[test_envelope("KeyRestored")],
            std::slice::from_ref(&key),
        )
        .await
        .unwrap();
    store
        .append_batch(&[PersistenceAppend {
            persistence_id: persistence_id.to_string(),
            expected_sequence: 5,
            events: vec![test_envelope("KeyRemoved")],
            key_rows: Some(Vec::new()),
        }])
        .await
        .unwrap();
    assert_eq!(
        store
            .lookup_by_key("default", "Order", "path", &key.key_hash)
            .await
            .unwrap(),
        None,
        "authoritative empty batch replacement must clear prior claims"
    );
}
