use temper_runtime::persistence::{
    EntityKeyRow, EntityVectorRow, EventMetadata, EventStore, IndexReconciliation,
    PersistenceAppend, PersistenceEnvelope,
};

use super::SimEventStore;

fn envelope(event_type: &str) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr: 0,
        event_type: event_type.to_string(),
        payload: serde_json::json!({"test": true}),
        metadata: EventMetadata {
            event_id: uuid::Uuid::nil(),
            causation_id: uuid::Uuid::nil(),
            correlation_id: uuid::Uuid::nil(),
            timestamp: chrono::DateTime::UNIX_EPOCH,
            actor_id: "batch-projection-test".to_string(),
        },
    }
}

fn exact_reconciliation() -> IndexReconciliation {
    IndexReconciliation {
        keys: true,
        vectors: true,
    }
}

#[tokio::test]
async fn indexed_batch_waits_for_fence_and_atomically_transfers_exact_rows() {
    let store = SimEventStore::no_faults(42);
    let key = EntityKeyRow {
        key_name: "number".to_string(),
        key_hash: "order-42".to_string(),
    };
    let old_vector = EntityVectorRow {
        decl_name: "embedding".to_string(),
        model_tag: "m1".to_string(),
        vector: vec![0.0, 1.0],
    };
    store
        .append_with_index_rows(
            "default:Order:old",
            0,
            &[envelope("Created")],
            std::slice::from_ref(&key),
            std::slice::from_ref(&old_vector),
            exact_reconciliation(),
        )
        .await
        .expect("seed old owner");

    let fence = store
        .acquire_projection_reconciliation_fence("default", "Order")
        .await
        .expect("acquire exclusive fence");
    let appends = [
        PersistenceAppend {
            persistence_id: "default:Order:old".to_string(),
            expected_sequence: 1,
            events: vec![envelope("Released")],
            key_rows: Vec::new(),
            vector_rows: Vec::new(),
            reconciliation: exact_reconciliation(),
        },
        PersistenceAppend {
            persistence_id: "default:Order:new".to_string(),
            expected_sequence: 0,
            events: vec![envelope("Created")],
            key_rows: vec![key.clone()],
            vector_rows: vec![EntityVectorRow {
                decl_name: "embedding".to_string(),
                model_tag: "m1".to_string(),
                vector: vec![1.0, 0.0],
            }],
            reconciliation: exact_reconciliation(),
        },
    ];
    let batch = store.append_batch(&appends);
    tokio::pin!(batch);

    let remained_blocked = tokio::select! {
        biased;
        result = &mut batch => panic!("indexed batch bypassed projection fence: {result:?}"),
        _ = tokio::task::yield_now() => true,
    };
    assert!(remained_blocked);

    drop(fence);
    let results = batch.await.expect("batch after fence release");
    assert_eq!(results[0].sequence_nr, 2);
    assert_eq!(results[1].sequence_nr, 1);
    assert_eq!(
        store
            .lookup_by_key("default", "Order", "number", "order-42")
            .await
            .expect("lookup transferred key")
            .as_deref(),
        Some("new")
    );
    let candidates = store
        .vector_candidates("default", "Order", "embedding", "m1", 10)
        .await
        .expect("read exact vectors");
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].entity_id, "new");
    assert_eq!(candidates[0].vector, vec![1.0, 0.0]);
    for (persistence_id, expected_end) in [("default:Order:old", 2), ("default:Order:new", 1)] {
        let segments = store.dump_segments(persistence_id);
        assert_eq!(segments.len(), 1, "batch append must open one segment");
        assert_eq!(segments[0].end_sequence_nr, Some(expected_end));
        assert_eq!(segments[0].event_count, expected_end);
    }

    let conflict = store
        .append_batch(&[PersistenceAppend {
            persistence_id: "default:Order:conflict".to_string(),
            expected_sequence: 0,
            events: vec![envelope("Created")],
            key_rows: vec![key],
            vector_rows: vec![old_vector],
            reconciliation: exact_reconciliation(),
        }])
        .await
        .expect_err("unreconciled current owner must reject duplicate key");
    assert!(conflict.to_string().contains("duplicate declared key"));
    assert!(store.dump_journal("default:Order:conflict").is_empty());
    assert_eq!(
        store
            .lookup_by_key("default", "Order", "number", "order-42")
            .await
            .expect("lookup after rejected batch")
            .as_deref(),
        Some("new")
    );
}
