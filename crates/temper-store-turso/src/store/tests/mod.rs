//! Integration tests for the Turso event store.

use libsql::params;
use temper_runtime::persistence::{
    EntityVectorRow, EventMetadata, EventStore, PersistenceAppend, PersistenceEnvelope,
    PersistenceError,
};

use super::{PublishedArtifactUpsert, QueryProjectionUpsert, TursoEventStore};
use crate::TursoSpecVerificationUpdate;

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
            actor_id: "store-test".to_string(),
        },
    }
}

fn sqlite_test_url(test_name: &str) -> String {
    let mut path = std::env::temp_dir();
    path.push(format!(
        "temper-store-turso-{test_name}-{}.db",
        uuid::Uuid::new_v4()
    ));
    format!("file:{}", path.display())
}

async fn make_store(test_name: &str) -> TursoEventStore {
    TursoEventStore::new(&sqlite_test_url(test_name), None)
        .await
        .expect("create store")
}

mod artifacts;
mod events;
mod listing;
mod projection_core;
mod projection_reads;
mod snapshots;
mod specs;
mod wasm;
