//! Unified event-store adapter for server runtime.
//!
//! `EventStore` is not dyn-object-safe in this workspace, so the server uses
//! a concrete enum to dispatch across backend implementations.

use sqlx::PgPool;
use temper_runtime::persistence::{EventStore, PersistenceEnvelope, PersistenceError};
use temper_store_postgres::PostgresEventStore;
use temper_store_redis::RedisEventStore;
use temper_store_turso::TursoEventStore;

#[cfg(feature = "sim")]
use temper_store_sim::SimEventStore;

/// Concrete event-store backend used by the server.
#[derive(Clone)]
pub enum ServerEventStore {
    Postgres(PostgresEventStore),
    Turso(TursoEventStore),
    Redis(RedisEventStore),
    /// In-memory deterministic event store for simulation testing.
    #[cfg(feature = "sim")]
    Sim(SimEventStore),
}

impl ServerEventStore {
    /// Human-readable backend name.
    pub fn backend_name(&self) -> &'static str {
        match self {
            Self::Postgres(_) => "postgres",
            Self::Turso(_) => "turso",
            Self::Redis(_) => "redis",
            #[cfg(feature = "sim")]
            Self::Sim(_) => "sim",
        }
    }

    /// Return the Postgres pool when using the Postgres backend.
    pub fn postgres_pool(&self) -> Option<&PgPool> {
        match self {
            Self::Postgres(store) => Some(store.pool()),
            Self::Turso(_) | Self::Redis(_) => None,
            #[cfg(feature = "sim")]
            Self::Sim(_) => None,
        }
    }

    /// Return the Turso store when using the Turso backend.
    pub fn turso_store(&self) -> Option<&TursoEventStore> {
        match self {
            Self::Turso(store) => Some(store),
            Self::Postgres(_) | Self::Redis(_) => None,
            #[cfg(feature = "sim")]
            Self::Sim(_) => None,
        }
    }

    /// Return the Redis store when using the Redis backend.
    pub fn redis_store(&self) -> Option<&RedisEventStore> {
        match self {
            Self::Redis(store) => Some(store),
            Self::Postgres(_) | Self::Turso(_) => None,
            #[cfg(feature = "sim")]
            Self::Sim(_) => None,
        }
    }
}

impl EventStore for ServerEventStore {
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        match self {
            Self::Postgres(store) => {
                store
                    .append(persistence_id, expected_sequence, events)
                    .await
            }
            Self::Turso(store) => {
                store
                    .append(persistence_id, expected_sequence, events)
                    .await
            }
            Self::Redis(store) => {
                store
                    .append(persistence_id, expected_sequence, events)
                    .await
            }
            #[cfg(feature = "sim")]
            Self::Sim(store) => {
                store
                    .append(persistence_id, expected_sequence, events)
                    .await
            }
        }
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        match self {
            Self::Postgres(store) => store.read_events(persistence_id, from_sequence).await,
            Self::Turso(store) => store.read_events(persistence_id, from_sequence).await,
            Self::Redis(store) => store.read_events(persistence_id, from_sequence).await,
            #[cfg(feature = "sim")]
            Self::Sim(store) => store.read_events(persistence_id, from_sequence).await,
        }
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        match self {
            Self::Postgres(store) => {
                store
                    .save_snapshot(persistence_id, sequence_nr, snapshot)
                    .await
            }
            Self::Turso(store) => {
                store
                    .save_snapshot(persistence_id, sequence_nr, snapshot)
                    .await
            }
            Self::Redis(store) => {
                store
                    .save_snapshot(persistence_id, sequence_nr, snapshot)
                    .await
            }
            #[cfg(feature = "sim")]
            Self::Sim(store) => {
                store
                    .save_snapshot(persistence_id, sequence_nr, snapshot)
                    .await
            }
        }
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        match self {
            Self::Postgres(store) => store.load_snapshot(persistence_id).await,
            Self::Turso(store) => store.load_snapshot(persistence_id).await,
            Self::Redis(store) => store.load_snapshot(persistence_id).await,
            #[cfg(feature = "sim")]
            Self::Sim(store) => store.load_snapshot(persistence_id).await,
        }
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        match self {
            Self::Postgres(store) => store.list_entity_ids(tenant).await,
            Self::Turso(store) => store.list_entity_ids(tenant).await,
            Self::Redis(store) => store.list_entity_ids(tenant).await,
            #[cfg(feature = "sim")]
            Self::Sim(store) => store.list_entity_ids(tenant).await,
        }
    }
}
