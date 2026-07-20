use super::*;
use crate::migration::run_migrations;
use sqlx::PgPool;
use temper_runtime::persistence::{EntityKeyRow, EventStore};

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
            actor_id: "store-projection-test".to_string(),
        },
    }
}

/// ADR-0153 live verification: the real postgres store honors the same
/// negative-existence + atomicity invariants the DST proved in SimEventStore.
/// Gated on DATABASE_URL (skips otherwise); isolated by a unique tenant.

#[path = "store_projection_test/keys.rs"]
mod keys;
#[path = "store_projection_test/queries.rs"]
mod queries;
#[path = "store_projection_test/updates.rs"]
mod updates;
