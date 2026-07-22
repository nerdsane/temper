//! Redis event-store integration regressions.

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

async fn seed_legacy_entity(
    store: &RedisEventStore,
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    mut event: PersistenceEnvelope,
) {
    event.sequence_nr = 1;
    let encoded = serde_json::to_string(&event).expect("encode legacy event");
    let _: i64 = store
        .client
        .rpush(
            RedisEventStore::events_key(tenant, entity_type, entity_id),
            encoded,
        )
        .await
        .expect("seed legacy journal");
    let _: () = store
        .client
        .set(
            RedisEventStore::seq_key(tenant, entity_type, entity_id),
            "1",
            None,
            None,
            false,
        )
        .await
        .expect("seed legacy sequence");
    let entity_ref = serde_json::to_string(&EntityRef {
        entity_type: entity_type.to_string(),
        entity_id: entity_id.to_string(),
    })
    .expect("encode legacy entity reference");
    let _: i64 = store
        .client
        .sadd(
            RedisEventStore::tenant_entities_key(tenant),
            vec![entity_ref],
        )
        .await
        .expect("seed legacy entity set");
}

#[path = "append.rs"]
mod append;
#[path = "basic.rs"]
mod basic;
#[path = "listing.rs"]
mod listing;
#[path = "migration.rs"]
mod migration;
#[path = "migration_boundedness.rs"]
mod migration_boundedness;
