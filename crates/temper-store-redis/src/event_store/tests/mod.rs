//! Redis event-store regression scenarios.

use super::*;
use temper_runtime::persistence::EventMetadata;

#[test]
fn batch_claim_key_is_unambiguous_across_entity_and_idempotency_boundaries() {
    let left = PersistenceBatchIdempotency {
        persistence_id: "tenant:Entity:a".to_string(),
        idempotency_key: "b:c".to_string(),
        intent_hash: "left".to_string(),
    };
    let right = PersistenceBatchIdempotency {
        persistence_id: "tenant:Entity:a:b".to_string(),
        idempotency_key: "c".to_string(),
        intent_hash: "right".to_string(),
    };

    assert_ne!(
        RedisEventStore::batch_idempotency_key(Some(&left)),
        RedisEventStore::batch_idempotency_key(Some(&right)),
        "claim ownership must bind the persistence id and idempotency key as distinct components"
    );
}

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

mod append;
mod listing;
mod materialization;
mod snapshot_segments;
mod source_fence;
