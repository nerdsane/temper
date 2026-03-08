//! Unified event-store adapter for server runtime.
//!
//! `EventStore` is not dyn-object-safe in this workspace, so the server uses
//! a concrete enum to dispatch across backend implementations.

use sqlx::PgPool;
use temper_runtime::persistence::{EventStore, PersistenceEnvelope, PersistenceError};
use temper_store_postgres::PostgresEventStore;
use temper_store_redis::RedisEventStore;
use temper_store_turso::{TenantStoreRouter, TursoEventStore};

#[cfg(feature = "sim")]
use temper_store_sim::SimEventStore;

/// Concrete event-store backend used by the server.
#[derive(Clone)]
pub enum ServerEventStore {
    Postgres(PostgresEventStore),
    Turso(TursoEventStore),
    Redis(RedisEventStore),
    /// Database-per-tenant routing via [`TenantStoreRouter`].
    TenantRouted(TenantStoreRouter),
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
            Self::TenantRouted(_) => "turso-routed",
            #[cfg(feature = "sim")]
            Self::Sim(_) => "sim",
        }
    }

    /// Return the Postgres pool when using the Postgres backend.
    pub fn postgres_pool(&self) -> Option<&PgPool> {
        match self {
            Self::Postgres(store) => Some(store.pool()),
            _ => None,
        }
    }

    /// Return the Turso store when using the single-DB Turso backend.
    ///
    /// Returns `None` in tenant-routed mode — use [`turso_for_tenant`] instead.
    pub fn turso_store(&self) -> Option<&TursoEventStore> {
        match self {
            Self::Turso(store) => Some(store),
            _ => None,
        }
    }

    /// Return the tenant store router when using database-per-tenant mode.
    pub fn tenant_router(&self) -> Option<&TenantStoreRouter> {
        match self {
            Self::TenantRouted(router) => Some(router),
            _ => None,
        }
    }

    /// Return a Turso store for a specific tenant.
    ///
    /// Works in both single-DB mode (returns the shared store) and
    /// tenant-routed mode (returns the per-tenant store).
    pub async fn turso_for_tenant(&self, tenant: &str) -> Option<TursoEventStore> {
        match self {
            Self::Turso(store) => Some(store.clone()),
            Self::TenantRouted(router) => router.store_for_tenant(tenant).await.ok(),
            _ => None,
        }
    }

    /// Return the Redis store when using the Redis backend.
    pub fn redis_store(&self) -> Option<&RedisEventStore> {
        match self {
            Self::Redis(store) => Some(store),
            _ => None,
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
            Self::TenantRouted(router) => {
                router
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
            Self::TenantRouted(router) => router.read_events(persistence_id, from_sequence).await,
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
            Self::TenantRouted(router) => {
                router
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
            Self::TenantRouted(router) => router.load_snapshot(persistence_id).await,
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
            Self::TenantRouted(router) => router.list_entity_ids(tenant).await,
            #[cfg(feature = "sim")]
            Self::Sim(store) => store.list_entity_ids(tenant).await,
        }
    }
}
