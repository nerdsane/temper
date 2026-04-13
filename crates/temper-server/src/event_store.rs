//! Unified event-store adapter for server runtime.
//!
//! `EventStore` is not dyn-object-safe in this workspace, so the server uses
//! a concrete enum to dispatch across backend implementations.

use sqlx::PgPool;
use temper_runtime::persistence::{EventStore, PersistenceEnvelope, PersistenceError};
use temper_store_postgres::PostgresEventStore;
use temper_store_redis::RedisEventStore;
use temper_store_turso::{TenantStoreRouter, TursoEventStore};

use crate::platform_store::PlatformStore;
#[cfg(feature = "sim")]
use crate::platform_store::SimPlatformStore;
#[cfg(feature = "sim")]
use std::sync::Arc;
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
    ///
    /// The optional [`SimPlatformStore`] is attached when the simulation needs
    /// platform-level storage (specs, OS apps, decisions, etc.).
    #[cfg(feature = "sim")]
    Sim(SimEventStore, Option<Arc<SimPlatformStore>>),
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
            Self::Sim(_, _) => "sim",
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

    /// Return the platform Turso store for shared tables (decisions, trajectories, etc.).
    ///
    /// Works in both single-DB mode (returns the shared store) and
    /// tenant-routed mode (returns the platform store from the router).
    pub fn platform_turso_store(&self) -> Option<&TursoEventStore> {
        match self {
            Self::Turso(store) => Some(store),
            Self::TenantRouted(router) => Some(router.platform_store()),
            _ => None,
        }
    }

    /// Return a reference to the platform store abstraction.
    ///
    /// Works for Turso (single-DB and tenant-routed), and Sim (when a
    /// `SimPlatformStore` is attached). Returns `None` for backends without
    /// platform-level storage (plain Postgres, Redis).
    pub fn platform_store(&self) -> Option<&dyn PlatformStore> {
        match self {
            Self::Turso(store) => Some(store as &dyn PlatformStore),
            Self::TenantRouted(router) => Some(router.platform_store() as &dyn PlatformStore),
            #[cfg(feature = "sim")]
            Self::Sim(_, Some(ps)) => Some(ps.as_ref() as &dyn PlatformStore),
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

impl ServerEventStore {
    /// Upsert the durable query-plane projection for an entity.
    pub async fn upsert_query_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        match self {
            Self::Turso(store) => {
                store
                    .upsert_query_projection(
                        tenant,
                        entity_type,
                        entity_id,
                        status,
                        fields,
                        sequence_nr,
                    )
                    .await
            }
            Self::TenantRouted(router) => {
                if let Ok(store) = router.store_for_tenant(tenant).await {
                    store
                        .upsert_query_projection(
                            tenant,
                            entity_type,
                            entity_id,
                            status,
                            fields,
                            sequence_nr,
                        )
                        .await
                } else {
                    Ok(()) // no tenant store → no-op
                }
            }
            _ => Ok(()), // query-plane projections only supported on Turso backends
        }
    }

    /// Remove the durable query-plane projection for an entity.
    pub async fn remove_query_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        match self {
            Self::Turso(store) => {
                store
                    .remove_query_projection(tenant, entity_type, entity_id)
                    .await
            }
            Self::TenantRouted(router) => {
                if let Ok(store) = router.store_for_tenant(tenant).await {
                    store
                        .remove_query_projection(tenant, entity_type, entity_id)
                        .await
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }

    /// Query the field index (delegates to Turso backends only).
    ///
    /// Returns `Ok(Some(ids))` when the backend supports field indexing and
    /// the query succeeded. Returns `Ok(None)` when the backend doesn't
    /// support field indexing (Postgres, Redis, Sim).
    pub async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        match self {
            Self::Turso(store) => store
                .query_field_index(tenant, entity_type, where_clause, params)
                .await
                .map(Some),
            Self::TenantRouted(router) => {
                if let Ok(store) = router.store_for_tenant(tenant).await {
                    store
                        .query_field_index(tenant, entity_type, where_clause, params)
                        .await
                        .map(Some)
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None), // field index only supported on Turso backends
        }
    }

    /// Return projected entity counts grouped by tenant.
    ///
    /// Returns `Ok(Some(counts))` when the backend supports query-plane projections.
    /// Returns `Ok(None)` for backends without a durable query plane.
    pub async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        match self {
            Self::Turso(store) => store.projected_entity_counts_by_tenant().await.map(Some),
            Self::TenantRouted(router) => {
                let mut counts = Vec::new();
                for tenant_id in router.connected_tenants().await {
                    if let Ok(store) = router.store_for_tenant(&tenant_id).await {
                        if let Some((_, count)) = store
                            .projected_entity_counts_by_tenant()
                            .await?
                            .into_iter()
                            .find(|(tenant, _)| tenant == &tenant_id)
                        {
                            counts.push((tenant_id.clone(), count));
                        }
                    }
                }
                Ok(Some(counts))
            }
            _ => Ok(None),
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
            Self::Sim(store, _) => {
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
            Self::Sim(store, _) => store.read_events(persistence_id, from_sequence).await,
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
            Self::Sim(store, _) => {
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
            Self::Sim(store, _) => store.load_snapshot(persistence_id).await,
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
            Self::Sim(store, _) => store.list_entity_ids(tenant).await,
        }
    }
}
