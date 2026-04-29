//! Unified event-store adapter for server runtime.
//!
//! `EventStore` is not dyn-object-safe in this workspace, so the server uses
//! a concrete enum to dispatch across backend implementations.

use sqlx::PgPool;
use temper_runtime::persistence::{EventStore, PersistenceEnvelope, PersistenceError};
use temper_store_postgres::PostgresEventStore;
use temper_store_postgres::PostgresTrajectoryInsert;
use temper_store_redis::RedisEventStore;
use temper_store_turso::TursoTrajectoryInsert;
use temper_store_turso::store::TrajectoryStats;
use temper_store_turso::store::field_index::ProjectedEntityFieldsRow;
use temper_store_turso::{
    ActionStats, AgentSummary, DesignTimeEventRow, EvolutionRecordRow, FeatureRequestRow,
    OtsTrajectoryParams, OtsTrajectoryRow, PolicyDenialPatternRow, TursoTrajectoryRow,
    TursoWasmInvocationInsert, TursoWasmInvocationRow, TursoWasmModuleMetadataRow,
    UnmetIntentAggRow,
};
use temper_store_turso::{TenantStoreRouter, TursoEventStore};

use crate::platform_store::PlatformStore;
#[cfg(feature = "sim")]
use crate::platform_store::SimPlatformStore;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use crate::storage::PolicyStoreRow;
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
    /// Works for Postgres, Turso (single-DB and tenant-routed), and Sim (when a
    /// `SimPlatformStore` is attached). Returns `None` for backends without
    /// platform-level storage (Redis).
    pub fn platform_store(&self) -> Option<&dyn PlatformStore> {
        match self {
            Self::Postgres(store) => Some(store as &dyn PlatformStore),
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
    pub async fn save_policy(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        match self {
            Self::Postgres(store) => store
                .save_policy(tenant, policy_id, cedar_text, created_by)
                .await
                .map_err(|e| e.to_string()),
            Self::Turso(store) => store
                .save_policy(tenant, policy_id, cedar_text, created_by)
                .await
                .map_err(|e| e.to_string()),
            Self::TenantRouted(router) => {
                let store = router
                    .store_for_tenant(tenant)
                    .await
                    .map_err(|e| e.to_string())?;
                store
                    .save_policy(tenant, policy_id, cedar_text, created_by)
                    .await
                    .map_err(|e| e.to_string())
            }
            Self::Redis(_) => Err(
                "Policy persistence is not supported on redis backend (explicit ephemeral mode: metadata is in-memory only)"
                    .to_string(),
            ),
            #[cfg(feature = "sim")]
            Self::Sim(_, _) => Ok(false),
        }
    }

    pub async fn load_policies_for_tenant(
        &self,
        tenant: &str,
    ) -> Result<Vec<PolicyStoreRow>, String> {
        match self {
            Self::Postgres(store) => store
                .load_policies_for_tenant(tenant)
                .await
                .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
                .map_err(|e| e.to_string()),
            Self::Turso(store) => store
                .load_policies_for_tenant(tenant)
                .await
                .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
                .map_err(|e| e.to_string()),
            Self::TenantRouted(router) => {
                let store = router
                    .store_for_tenant(tenant)
                    .await
                    .map_err(|e| e.to_string())?;
                store
                    .load_policies_for_tenant(tenant)
                    .await
                    .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
                    .map_err(|e| e.to_string())
            }
            Self::Redis(_) => Err(
                "Policy reads are not supported on redis backend (explicit ephemeral mode: metadata is in-memory only)"
                    .to_string(),
            ),
            #[cfg(feature = "sim")]
            Self::Sim(_, _) => Ok(Vec::new()),
        }
    }

    pub async fn load_all_policies(&self) -> Result<Vec<PolicyStoreRow>, String> {
        match self {
            Self::Postgres(store) => store
                .load_all_policies()
                .await
                .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
                .map_err(|e| e.to_string()),
            Self::Turso(store) => store
                .load_all_policies()
                .await
                .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
                .map_err(|e| e.to_string()),
            Self::TenantRouted(router) => {
                let mut rows: Vec<PolicyStoreRow> = router
                    .platform_store()
                    .load_all_policies()
                    .await
                    .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
                    .map_err(|e| e.to_string())?;
                for tenant_id in router.connected_tenants().await {
                    if let Ok(store) = router.store_for_tenant(&tenant_id).await {
                        let mut tenant_rows: Vec<PolicyStoreRow> = store
                            .load_all_policies()
                            .await
                            .map(|rows| rows.into_iter().map(PolicyStoreRow::from).collect())
                            .map_err(|e| e.to_string())?;
                        rows.append(&mut tenant_rows);
                    }
                }
                Ok(rows)
            }
            Self::Redis(_) => Err(
                "Policy reads are not supported on redis backend (explicit ephemeral mode: metadata is in-memory only)"
                    .to_string(),
            ),
            #[cfg(feature = "sim")]
            Self::Sim(_, _) => Ok(Vec::new()),
        }
    }

    pub async fn toggle_policy_enabled(
        &self,
        tenant: &str,
        policy_id: &str,
        enabled: bool,
    ) -> Result<bool, String> {
        match self {
            Self::Postgres(store) => store
                .toggle_policy_enabled(tenant, policy_id, enabled)
                .await
                .map_err(|e| e.to_string()),
            Self::Turso(store) => store
                .toggle_policy_enabled(tenant, policy_id, enabled)
                .await
                .map_err(|e| e.to_string()),
            Self::TenantRouted(router) => {
                let store = router
                    .store_for_tenant(tenant)
                    .await
                    .map_err(|e| e.to_string())?;
                store
                    .toggle_policy_enabled(tenant, policy_id, enabled)
                    .await
                    .map_err(|e| e.to_string())
            }
            Self::Redis(_) => Err(
                "Policy persistence is not supported on redis backend (explicit ephemeral mode: metadata is in-memory only)"
                    .to_string(),
            ),
            #[cfg(feature = "sim")]
            Self::Sim(_, _) => Ok(false),
        }
    }

    pub async fn update_policy_text(
        &self,
        tenant: &str,
        policy_id: &str,
        cedar_text: &str,
        created_by: &str,
    ) -> Result<bool, String> {
        match self {
            Self::Postgres(store) => store
                .update_policy_text(tenant, policy_id, cedar_text, created_by)
                .await
                .map_err(|e| e.to_string()),
            Self::Turso(store) => store
                .update_policy_text(tenant, policy_id, cedar_text, created_by)
                .await
                .map_err(|e| e.to_string()),
            Self::TenantRouted(router) => {
                let store = router
                    .store_for_tenant(tenant)
                    .await
                    .map_err(|e| e.to_string())?;
                store
                    .update_policy_text(tenant, policy_id, cedar_text, created_by)
                    .await
                    .map_err(|e| e.to_string())
            }
            Self::Redis(_) => Err(
                "Policy persistence is not supported on redis backend (explicit ephemeral mode: metadata is in-memory only)"
                    .to_string(),
            ),
            #[cfg(feature = "sim")]
            Self::Sim(_, _) => Ok(false),
        }
    }

    pub async fn delete_policy(&self, tenant: &str, policy_id: &str) -> Result<(), String> {
        match self {
            Self::Postgres(store) => store
                .delete_policy(tenant, policy_id)
                .await
                .map_err(|e| e.to_string()),
            Self::Turso(store) => store
                .delete_policy(tenant, policy_id)
                .await
                .map_err(|e| e.to_string()),
            Self::TenantRouted(router) => {
                let store = router
                    .store_for_tenant(tenant)
                    .await
                    .map_err(|e| e.to_string())?;
                store
                    .delete_policy(tenant, policy_id)
                    .await
                    .map_err(|e| e.to_string())
            }
            Self::Redis(_) => Err(
                "Policy persistence is not supported on redis backend (explicit ephemeral mode: metadata is in-memory only)"
                    .to_string(),
            ),
            #[cfg(feature = "sim")]
            Self::Sim(_, _) => Ok(()),
        }
    }

    /// Persist one observe trajectory entry to the durable metadata backend.
    pub async fn persist_trajectory_entry(&self, entry: &TrajectoryEntry) -> Result<(), String> {
        let matched_policy_ids_json = entry
            .matched_policy_ids
            .as_ref()
            .map(|ids| serde_json::to_string(ids).unwrap_or_default());
        let request_body_json = entry.request_body.as_ref().and_then(|value| {
            let serialized = serde_json::to_string(value).ok()?;
            Some(if serialized.len() > 4096 {
                let mut end = 4096;
                while !serialized.is_char_boundary(end) {
                    end -= 1;
                }
                serialized[..end].to_string()
            } else {
                serialized
            })
        });
        let source = entry.source.as_ref().map(|source| match source {
            TrajectorySource::Entity => "Entity",
            TrajectorySource::Platform => "Platform",
            TrajectorySource::Authz => "Authz",
        });

        match self {
            Self::Postgres(store) => store
                .persist_trajectory(PostgresTrajectoryInsert {
                    tenant: &entry.tenant,
                    entity_type: &entry.entity_type,
                    entity_id: &entry.entity_id,
                    action: &entry.action,
                    success: entry.success,
                    from_status: entry.from_status.as_deref(),
                    to_status: entry.to_status.as_deref(),
                    error: entry.error.as_deref(),
                    agent_id: entry.agent_id.as_deref(),
                    session_id: entry.session_id.as_deref(),
                    authz_denied: entry.authz_denied,
                    denied_resource: entry.denied_resource.as_deref(),
                    denied_module: entry.denied_module.as_deref(),
                    source,
                    spec_governed: entry.spec_governed,
                    created_at: &entry.timestamp,
                    request_body: request_body_json.as_deref(),
                    intent: entry.intent.as_deref(),
                    matched_policy_ids: matched_policy_ids_json.as_deref(),
                })
                .await
                .map_err(|e| {
                    format!(
                        "failed to persist trajectory entry for {}/{}/{} action {} in postgres: {e}",
                        entry.tenant, entry.entity_type, entry.entity_id, entry.action
                    )
                }),
            Self::Turso(store) => store
                .persist_trajectory(TursoTrajectoryInsert {
                    tenant: &entry.tenant,
                    entity_type: &entry.entity_type,
                    entity_id: &entry.entity_id,
                    action: &entry.action,
                    success: entry.success,
                    from_status: entry.from_status.as_deref(),
                    to_status: entry.to_status.as_deref(),
                    error: entry.error.as_deref(),
                    agent_id: entry.agent_id.as_deref(),
                    session_id: entry.session_id.as_deref(),
                    authz_denied: entry.authz_denied,
                    denied_resource: entry.denied_resource.as_deref(),
                    denied_module: entry.denied_module.as_deref(),
                    source,
                    spec_governed: entry.spec_governed,
                    created_at: &entry.timestamp,
                    request_body: request_body_json.as_deref(),
                    intent: entry.intent.as_deref(),
                    matched_policy_ids: matched_policy_ids_json.as_deref(),
                })
                .await
                .map_err(|e| {
                    format!(
                        "failed to persist trajectory entry for {}/{}/{} action {} in turso: {e}",
                        entry.tenant, entry.entity_type, entry.entity_id, entry.action
                    )
                }),
            Self::TenantRouted(router) => {
                let store = router.store_for_tenant(&entry.tenant).await.map_err(|e| {
                    format!(
                        "failed to resolve tenant store for trajectory entry {}/{}/{} action {}: {e}",
                        entry.tenant, entry.entity_type, entry.entity_id, entry.action
                    )
                })?;
                store
                    .persist_trajectory(TursoTrajectoryInsert {
                        tenant: &entry.tenant,
                        entity_type: &entry.entity_type,
                        entity_id: &entry.entity_id,
                        action: &entry.action,
                        success: entry.success,
                        from_status: entry.from_status.as_deref(),
                        to_status: entry.to_status.as_deref(),
                        error: entry.error.as_deref(),
                        agent_id: entry.agent_id.as_deref(),
                        session_id: entry.session_id.as_deref(),
                        authz_denied: entry.authz_denied,
                        denied_resource: entry.denied_resource.as_deref(),
                        denied_module: entry.denied_module.as_deref(),
                        source,
                        spec_governed: entry.spec_governed,
                        created_at: &entry.timestamp,
                        request_body: request_body_json.as_deref(),
                        intent: entry.intent.as_deref(),
                        matched_policy_ids: matched_policy_ids_json.as_deref(),
                    })
                    .await
                    .map_err(|e| {
                        format!(
                            "failed to persist trajectory entry for {}/{}/{} action {} in turso-routed: {e}",
                            entry.tenant, entry.entity_type, entry.entity_id, entry.action
                        )
                    })
            }
            Self::Redis(_) => Err(
                "Trajectory persistence is not supported on redis backend (explicit ephemeral mode: metadata is in-memory only)"
                    .to_string(),
            ),
            #[cfg(feature = "sim")]
            Self::Sim(_, _) => Ok(()),
        }
    }

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
            Self::Postgres(store) => {
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
            _ => Ok(()), // Redis/sim have no durable query plane
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
            Self::Postgres(store) => {
                store
                    .remove_query_projection(tenant, entity_type, entity_id)
                    .await
            }
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

    /// Query the field index.
    ///
    /// Returns `Ok(Some(ids))` when the backend supports field indexing and
    /// the query succeeded. Returns `Ok(None)` when the backend doesn't
    /// support field indexing (Redis, Sim).
    pub async fn query_field_index(
        &self,
        tenant: &str,
        entity_type: &str,
        where_clause: &str,
        params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .query_field_index(tenant, entity_type, where_clause, params)
                .await
                .map(Some),
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
            _ => Ok(None), // field index unsupported on Redis/sim
        }
    }

    pub async fn load_query_projection_fields_many(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
        field_names: &[&str],
    ) -> Result<Option<Vec<ProjectedEntityFieldsRow>>, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .load_query_projection_fields_many(tenant, entity_type, entity_ids, field_names)
                .await
                .map(|rows| {
                    Some(
                        rows.into_iter()
                            .map(|row| ProjectedEntityFieldsRow {
                                entity_id: row.entity_id,
                                status: row.status,
                                fields: row.fields,
                            })
                            .collect(),
                    )
                }),
            Self::Turso(store) => store
                .load_query_projection_fields_many(tenant, entity_type, entity_ids, field_names)
                .await
                .map(Some),
            Self::TenantRouted(router) => {
                if let Ok(store) = router.store_for_tenant(tenant).await {
                    store
                        .load_query_projection_fields_many(
                            tenant,
                            entity_type,
                            entity_ids,
                            field_names,
                        )
                        .await
                        .map(Some)
                } else {
                    Ok(None)
                }
            }
            _ => Ok(None),
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
            Self::Postgres(store) => store.projected_entity_counts_by_tenant().await.map(Some),
            Self::Turso(store) => store.projected_entity_counts_by_tenant().await.map(Some),
            Self::TenantRouted(router) => {
                let mut counts = Vec::new();
                for tenant_id in router.connected_tenants().await {
                    if let Ok(store) = router.store_for_tenant(&tenant_id).await
                        && let Some((_, count)) = store
                            .projected_entity_counts_by_tenant()
                            .await?
                            .into_iter()
                            .find(|(tenant, _)| tenant == &tenant_id)
                    {
                        counts.push((tenant_id.clone(), count));
                    }
                }
                Ok(Some(counts))
            }
            _ => Ok(None),
        }
    }

    pub async fn load_recent_trajectories(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .load_recent_trajectories(limit)
                .await
                .map(|rows| rows.into_iter().map(pg_trajectory_to_turso).collect()),
            Self::Turso(store) => store.load_recent_trajectories(limit).await,
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .load_recent_trajectories(limit)
                    .await
            }
            _ => Ok(Vec::new()),
        }
    }

    pub async fn load_unmet_intent_rows(&self) -> Result<Vec<UnmetIntentAggRow>, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .load_unmet_intent_rows()
                .await
                .map(|rows| rows.into_iter().map(pg_unmet_to_turso).collect()),
            Self::Turso(store) => store.load_unmet_intent_rows().await,
            Self::TenantRouted(router) => router.platform_store().load_unmet_intent_rows().await,
            _ => Ok(Vec::new()),
        }
    }

    pub async fn load_submit_spec_timestamps(
        &self,
    ) -> Result<std::collections::BTreeMap<String, String>, PersistenceError> {
        match self {
            Self::Postgres(store) => store.load_submit_spec_timestamps().await,
            Self::Turso(store) => store.load_submit_spec_timestamps().await,
            Self::TenantRouted(router) => {
                router.platform_store().load_submit_spec_timestamps().await
            }
            _ => Ok(std::collections::BTreeMap::new()),
        }
    }

    pub async fn count_trajectories_by_tenant(
        &self,
    ) -> Result<std::collections::BTreeMap<String, u64>, PersistenceError> {
        match self {
            Self::Postgres(store) => store.count_trajectories_by_tenant().await,
            Self::Turso(store) => store.count_trajectories_by_tenant().await,
            Self::TenantRouted(router) => {
                router.platform_store().count_trajectories_by_tenant().await
            }
            _ => Ok(std::collections::BTreeMap::new()),
        }
    }

    pub async fn query_trajectory_stats(
        &self,
        entity_type: Option<&str>,
        action: Option<&str>,
        success_filter: Option<bool>,
        failed_limit: i64,
    ) -> Result<TrajectoryStats, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .query_trajectory_stats(entity_type, action, success_filter, failed_limit)
                .await
                .map(pg_stats_to_turso),
            Self::Turso(store) => {
                store
                    .query_trajectory_stats(entity_type, action, success_filter, failed_limit)
                    .await
            }
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .query_trajectory_stats(entity_type, action, success_filter, failed_limit)
                    .await
            }
            _ => Ok(TrajectoryStats {
                total: 0,
                success_count: 0,
                error_count: 0,
                success_rate: 0.0,
                by_action: std::collections::BTreeMap::new(),
                failed_intents: Vec::new(),
            }),
        }
    }

    pub async fn query_trajectories_by_agent(
        &self,
        agent_id: &str,
        tenant: Option<&str>,
        entity_type: Option<&str>,
        limit: i64,
    ) -> Result<Vec<TursoTrajectoryRow>, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .query_trajectories_by_agent(agent_id, tenant, entity_type, limit)
                .await
                .map(|rows| rows.into_iter().map(pg_trajectory_to_turso).collect()),
            Self::Turso(store) => {
                store
                    .query_trajectories_by_agent(agent_id, tenant, entity_type, limit)
                    .await
            }
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .query_trajectories_by_agent(agent_id, tenant, entity_type, limit)
                    .await
            }
            _ => Ok(Vec::new()),
        }
    }

    pub async fn query_agent_summaries(
        &self,
        tenant: Option<&str>,
    ) -> Result<Vec<AgentSummary>, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .query_agent_summaries(tenant)
                .await
                .map(|rows| rows.into_iter().map(pg_agent_summary_to_turso).collect()),
            Self::Turso(store) => store.query_agent_summaries(tenant).await,
            Self::TenantRouted(router) => {
                router.platform_store().query_agent_summaries(tenant).await
            }
            _ => Ok(Vec::new()),
        }
    }

    pub async fn upsert_feature_request(
        &self,
        id: &str,
        category: &str,
        description: &str,
        frequency: i64,
        trajectory_refs_json: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<(), PersistenceError> {
        match self {
            Self::Postgres(store) => {
                store
                    .upsert_feature_request(
                        id,
                        category,
                        description,
                        frequency,
                        trajectory_refs_json,
                        disposition,
                        developer_notes,
                    )
                    .await
            }
            Self::Turso(store) => {
                store
                    .upsert_feature_request(
                        id,
                        category,
                        description,
                        frequency,
                        trajectory_refs_json,
                        disposition,
                        developer_notes,
                    )
                    .await
            }
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .upsert_feature_request(
                        id,
                        category,
                        description,
                        frequency,
                        trajectory_refs_json,
                        disposition,
                        developer_notes,
                    )
                    .await
            }
            _ => Ok(()),
        }
    }

    pub async fn list_feature_requests(
        &self,
        disposition: Option<&str>,
    ) -> Result<Vec<FeatureRequestRow>, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .list_feature_requests(disposition)
                .await
                .map(|rows| rows.into_iter().map(pg_feature_request_to_turso).collect()),
            Self::Turso(store) => store.list_feature_requests(disposition).await,
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .list_feature_requests(disposition)
                    .await
            }
            _ => Ok(Vec::new()),
        }
    }

    pub async fn update_feature_request(
        &self,
        id: &str,
        disposition: &str,
        developer_notes: Option<&str>,
    ) -> Result<bool, PersistenceError> {
        match self {
            Self::Postgres(store) => {
                store
                    .update_feature_request(id, disposition, developer_notes)
                    .await
            }
            Self::Turso(store) => {
                store
                    .update_feature_request(id, disposition, developer_notes)
                    .await
            }
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .update_feature_request(id, disposition, developer_notes)
                    .await
            }
            _ => Ok(false),
        }
    }

    pub async fn insert_evolution_record(
        &self,
        id: &str,
        record_type: &str,
        status: &str,
        created_by: &str,
        derived_from: Option<&str>,
        data_json: &str,
    ) -> Result<(), PersistenceError> {
        match self {
            Self::Postgres(store) => {
                store
                    .insert_evolution_record(
                        id,
                        record_type,
                        status,
                        created_by,
                        derived_from,
                        data_json,
                    )
                    .await
            }
            Self::Turso(store) => {
                store
                    .insert_evolution_record(
                        id,
                        record_type,
                        status,
                        created_by,
                        derived_from,
                        data_json,
                    )
                    .await
            }
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .insert_evolution_record(
                        id,
                        record_type,
                        status,
                        created_by,
                        derived_from,
                        data_json,
                    )
                    .await
            }
            _ => Ok(()),
        }
    }

    pub async fn get_evolution_record(
        &self,
        id: &str,
    ) -> Result<Option<EvolutionRecordRow>, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .get_evolution_record(id)
                .await
                .map(|row| row.map(pg_evolution_record_to_turso)),
            Self::Turso(store) => store.get_evolution_record(id).await,
            Self::TenantRouted(router) => router.platform_store().get_evolution_record(id).await,
            _ => Ok(None),
        }
    }

    pub async fn list_evolution_records(
        &self,
        record_type: Option<&str>,
        status: Option<&str>,
    ) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .list_evolution_records(record_type, status)
                .await
                .map(|rows| rows.into_iter().map(pg_evolution_record_to_turso).collect()),
            Self::Turso(store) => store.list_evolution_records(record_type, status).await,
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .list_evolution_records(record_type, status)
                    .await
            }
            _ => Ok(Vec::new()),
        }
    }

    pub async fn list_ranked_insights(&self) -> Result<Vec<EvolutionRecordRow>, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .list_ranked_insights()
                .await
                .map(|rows| rows.into_iter().map(pg_evolution_record_to_turso).collect()),
            Self::Turso(store) => store.list_ranked_insights().await,
            Self::TenantRouted(router) => router.platform_store().list_ranked_insights().await,
            _ => Ok(Vec::new()),
        }
    }

    pub async fn insert_design_time_event(
        &self,
        kind: &str,
        entity_type: &str,
        tenant: &str,
        summary: &str,
        level: Option<&str>,
        passed: Option<bool>,
        step_number: Option<i64>,
        total_steps: Option<i64>,
    ) -> Result<(), PersistenceError> {
        match self {
            Self::Postgres(store) => {
                store
                    .insert_design_time_event(
                        kind,
                        entity_type,
                        tenant,
                        summary,
                        level,
                        passed,
                        step_number,
                        total_steps,
                    )
                    .await
            }
            Self::Turso(store) => {
                store
                    .insert_design_time_event(
                        kind,
                        entity_type,
                        tenant,
                        summary,
                        level,
                        passed,
                        step_number,
                        total_steps,
                    )
                    .await
            }
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .insert_design_time_event(
                        kind,
                        entity_type,
                        tenant,
                        summary,
                        level,
                        passed,
                        step_number,
                        total_steps,
                    )
                    .await
            }
            _ => Ok(()),
        }
    }

    pub async fn list_design_time_events(
        &self,
        tenant: Option<&str>,
        limit: i64,
    ) -> Result<Vec<DesignTimeEventRow>, PersistenceError> {
        match self {
            Self::Postgres(store) => {
                store
                    .list_design_time_events(tenant, limit)
                    .await
                    .map(|rows| {
                        rows.into_iter()
                            .map(pg_design_time_event_to_turso)
                            .collect()
                    })
            }
            Self::Turso(store) => store.list_design_time_events(tenant, limit).await,
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .list_design_time_events(tenant, limit)
                    .await
            }
            _ => Ok(Vec::new()),
        }
    }

    pub async fn persist_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        match self {
            Self::Postgres(store) => {
                store
                    .persist_ots_trajectory(&temper_store_postgres::PostgresOtsTrajectoryParams {
                        trajectory_id: params.trajectory_id,
                        tenant: params.tenant,
                        agent_id: params.agent_id,
                        session_id: params.session_id,
                        outcome: params.outcome,
                        turn_count: params.turn_count,
                        data: params.data,
                    })
                    .await
            }
            Self::Turso(store) => store.persist_ots_trajectory(params).await,
            Self::TenantRouted(router) => {
                router.platform_store().persist_ots_trajectory(params).await
            }
            _ => Ok(()),
        }
    }

    pub async fn list_ots_trajectories(
        &self,
        tenant: &str,
        agent_id: Option<&str>,
        outcome: Option<&str>,
        limit: i64,
    ) -> Result<Vec<OtsTrajectoryRow>, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .list_ots_trajectories(tenant, agent_id, outcome, limit)
                .await
                .map(|rows| rows.into_iter().map(pg_ots_to_turso).collect()),
            Self::Turso(store) => {
                store
                    .list_ots_trajectories(tenant, agent_id, outcome, limit)
                    .await
            }
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .list_ots_trajectories(tenant, agent_id, outcome, limit)
                    .await
            }
            _ => Ok(Vec::new()),
        }
    }

    pub async fn get_ots_trajectory(
        &self,
        trajectory_id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        match self {
            Self::Postgres(store) => store.get_ots_trajectory(trajectory_id).await,
            Self::Turso(store) => store.get_ots_trajectory(trajectory_id).await,
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .get_ots_trajectory(trajectory_id)
                    .await
            }
            _ => Ok(None),
        }
    }

    pub async fn put_blob(&self, key: &str, data: &[u8]) -> Result<(), String> {
        match self {
            Self::Postgres(store) => store.put_blob(key, data).await,
            Self::Turso(store) => store.put_blob(key, data).await,
            Self::TenantRouted(router) => router.platform_store().put_blob(key, data).await,
            Self::Redis(_) => Err("Blob persistence is not supported on redis backend".to_string()),
            #[cfg(feature = "sim")]
            Self::Sim(_, _) => Ok(()),
        }
    }

    pub async fn put_blob_with_ttl(
        &self,
        key: &str,
        data: &[u8],
        ttl: Option<std::time::Duration>,
    ) -> Result<(), String> {
        match self {
            Self::Postgres(store) => store.put_blob_with_ttl(key, data, ttl).await,
            Self::Turso(store) => store.put_blob_with_ttl(key, data, ttl).await,
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .put_blob_with_ttl(key, data, ttl)
                    .await
            }
            Self::Redis(_) => Err("Blob persistence is not supported on redis backend".to_string()),
            #[cfg(feature = "sim")]
            Self::Sim(_, _) => Ok(()),
        }
    }

    pub async fn sweep_expired_blobs(&self, max_rows: u64) -> Result<u64, String> {
        match self {
            Self::Postgres(store) => store.sweep_expired_blobs(max_rows).await,
            Self::Turso(store) => store.sweep_expired_blobs(max_rows).await,
            Self::TenantRouted(router) => {
                router.platform_store().sweep_expired_blobs(max_rows).await
            }
            Self::Redis(_) => Ok(0),
            #[cfg(feature = "sim")]
            Self::Sim(_, _) => Ok(0),
        }
    }

    pub async fn get_blob(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        match self {
            Self::Postgres(store) => store.get_blob(key).await,
            Self::Turso(store) => store.get_blob(key).await,
            Self::TenantRouted(router) => router.platform_store().get_blob(key).await,
            Self::Redis(_) => Ok(None),
            #[cfg(feature = "sim")]
            Self::Sim(_, _) => Ok(None),
        }
    }

    pub async fn upsert_policy_denial_pattern(
        &self,
        tenant: &str,
        agent_type: Option<&str>,
        action: &str,
        resource_type: &str,
        resource_id: &str,
        timestamp: &str,
    ) -> Result<(), PersistenceError> {
        match self {
            Self::Postgres(store) => {
                store
                    .upsert_policy_denial_pattern(
                        tenant,
                        agent_type,
                        action,
                        resource_type,
                        resource_id,
                        timestamp,
                    )
                    .await
            }
            Self::Turso(store) => {
                store
                    .upsert_policy_denial_pattern(
                        tenant,
                        agent_type,
                        action,
                        resource_type,
                        resource_id,
                        timestamp,
                    )
                    .await
            }
            Self::TenantRouted(router) => {
                let store = router.store_for_tenant(tenant).await.map_err(|e| {
                    PersistenceError::Storage(format!("failed to resolve tenant store: {e}"))
                })?;
                store
                    .upsert_policy_denial_pattern(
                        tenant,
                        agent_type,
                        action,
                        resource_type,
                        resource_id,
                        timestamp,
                    )
                    .await
            }
            _ => Ok(()),
        }
    }

    pub async fn load_policy_denial_patterns(
        &self,
        tenant: &str,
    ) -> Result<Vec<PolicyDenialPatternRow>, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .load_policy_denial_patterns(tenant)
                .await
                .map(|rows| rows.into_iter().map(pg_denial_pattern_to_turso).collect()),
            Self::Turso(store) => store.load_policy_denial_patterns(tenant).await,
            Self::TenantRouted(router) => {
                let store = router.store_for_tenant(tenant).await.map_err(|e| {
                    PersistenceError::Storage(format!("failed to resolve tenant store: {e}"))
                })?;
                store.load_policy_denial_patterns(tenant).await
            }
            _ => Ok(Vec::new()),
        }
    }

    pub async fn query_decisions(
        &self,
        tenant: &str,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        match self {
            Self::Postgres(store) => store.query_decisions(tenant, status).await,
            Self::Turso(store) => store.query_decisions(tenant, status).await,
            Self::TenantRouted(router) => {
                let store = router.store_for_tenant(tenant).await.map_err(|e| {
                    PersistenceError::Storage(format!("failed to resolve tenant store: {e}"))
                })?;
                store.query_decisions(tenant, status).await
            }
            _ => Ok(Vec::new()),
        }
    }

    pub async fn query_all_decisions(
        &self,
        status: Option<&str>,
    ) -> Result<Vec<String>, PersistenceError> {
        match self {
            Self::Postgres(store) => store.query_all_decisions(status).await,
            Self::Turso(store) => store.query_all_decisions(status).await,
            Self::TenantRouted(router) => router.platform_store().query_all_decisions(status).await,
            _ => Ok(Vec::new()),
        }
    }

    pub async fn get_pending_decision(&self, id: &str) -> Result<Option<String>, PersistenceError> {
        match self {
            Self::Postgres(store) => store.get_pending_decision(id).await,
            Self::Turso(store) => store.get_pending_decision(id).await,
            Self::TenantRouted(router) => router.platform_store().get_pending_decision(id).await,
            _ => Ok(None),
        }
    }

    pub async fn load_wasm_module_metadata_all_tenants(
        &self,
    ) -> Result<Vec<TursoWasmModuleMetadataRow>, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .load_wasm_module_metadata_all_tenants()
                .await
                .map(|rows| rows.into_iter().map(pg_wasm_metadata_to_turso).collect()),
            Self::Turso(store) => store.load_wasm_module_metadata_all_tenants().await,
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .load_wasm_module_metadata_all_tenants()
                    .await
            }
            _ => Ok(Vec::new()),
        }
    }

    pub async fn persist_wasm_invocation(
        &self,
        entry: &TursoWasmInvocationInsert<'_>,
    ) -> Result<(), PersistenceError> {
        match self {
            Self::Postgres(store) => {
                store
                    .persist_wasm_invocation(&temper_store_postgres::PostgresWasmInvocationInsert {
                        tenant: entry.tenant,
                        entity_type: entry.entity_type,
                        entity_id: entry.entity_id,
                        module_name: entry.module_name,
                        trigger_action: entry.trigger_action,
                        callback_action: entry.callback_action,
                        success: entry.success,
                        error: entry.error,
                        duration_ms: entry.duration_ms,
                        created_at: entry.created_at,
                    })
                    .await
            }
            Self::Turso(store) => store.persist_wasm_invocation(entry).await,
            Self::TenantRouted(router) => {
                let store = router.store_for_tenant(entry.tenant).await.map_err(|e| {
                    PersistenceError::Storage(format!("failed to resolve tenant store: {e}"))
                })?;
                store.persist_wasm_invocation(entry).await
            }
            _ => Ok(()),
        }
    }

    pub async fn load_recent_wasm_invocations(
        &self,
        limit: i64,
    ) -> Result<Vec<TursoWasmInvocationRow>, PersistenceError> {
        match self {
            Self::Postgres(store) => store
                .load_recent_wasm_invocations(limit)
                .await
                .map(|rows| rows.into_iter().map(pg_wasm_invocation_to_turso).collect()),
            Self::Turso(store) => store.load_recent_wasm_invocations(limit).await,
            Self::TenantRouted(router) => {
                router
                    .platform_store()
                    .load_recent_wasm_invocations(limit)
                    .await
            }
            _ => Ok(Vec::new()),
        }
    }

    pub async fn delete_wasm_module(
        &self,
        tenant: &str,
        module_name: &str,
    ) -> Result<bool, PersistenceError> {
        match self {
            Self::Postgres(store) => store.delete_wasm_module(tenant, module_name).await,
            Self::Turso(store) => store.delete_wasm_module(tenant, module_name).await,
            Self::TenantRouted(router) => {
                let store = router.store_for_tenant(tenant).await.map_err(|e| {
                    PersistenceError::Storage(format!("failed to resolve tenant store: {e}"))
                })?;
                store.delete_wasm_module(tenant, module_name).await
            }
            _ => Ok(false),
        }
    }
}

fn pg_trajectory_to_turso(row: temper_store_postgres::PostgresTrajectoryRow) -> TursoTrajectoryRow {
    TursoTrajectoryRow {
        tenant: row.tenant,
        entity_type: row.entity_type,
        entity_id: row.entity_id,
        action: row.action,
        success: row.success,
        from_status: row.from_status,
        to_status: row.to_status,
        error: row.error,
        agent_id: row.agent_id,
        session_id: row.session_id,
        authz_denied: row.authz_denied,
        denied_resource: row.denied_resource,
        denied_module: row.denied_module,
        source: row.source,
        spec_governed: row.spec_governed,
        created_at: row.created_at,
        request_body: row.request_body,
        intent: row.intent,
        matched_policy_ids: row.matched_policy_ids,
    }
}

fn pg_unmet_to_turso(row: temper_store_postgres::PostgresUnmetIntentAggRow) -> UnmetIntentAggRow {
    UnmetIntentAggRow {
        entity_type: row.entity_type,
        action: row.action,
        error: row.error,
        count: row.count,
        first_seen: row.first_seen,
        last_seen: row.last_seen,
    }
}

fn pg_stats_to_turso(stats: temper_store_postgres::PostgresTrajectoryStats) -> TrajectoryStats {
    TrajectoryStats {
        total: stats.total,
        success_count: stats.success_count,
        error_count: stats.error_count,
        success_rate: stats.success_rate,
        by_action: stats
            .by_action
            .into_iter()
            .map(|(name, action)| {
                (
                    name,
                    ActionStats {
                        total: action.total,
                        success: action.success,
                        error: action.error,
                    },
                )
            })
            .collect(),
        failed_intents: stats
            .failed_intents
            .into_iter()
            .map(pg_trajectory_to_turso)
            .collect(),
    }
}

fn pg_agent_summary_to_turso(row: temper_store_postgres::PostgresAgentSummary) -> AgentSummary {
    AgentSummary {
        agent_id: row.agent_id,
        total_actions: row.total_actions,
        success_count: row.success_count,
        error_count: row.error_count,
        denial_count: row.denial_count,
        success_rate: row.success_rate,
        last_active_at: row.last_active_at,
    }
}

fn pg_feature_request_to_turso(
    row: temper_store_postgres::PostgresFeatureRequestRow,
) -> FeatureRequestRow {
    FeatureRequestRow {
        id: row.id,
        category: row.category,
        description: row.description,
        frequency: row.frequency,
        trajectory_refs: row.trajectory_refs,
        disposition: row.disposition,
        developer_notes: row.developer_notes,
        created_at: row.created_at,
        updated_at: row.updated_at,
    }
}

fn pg_evolution_record_to_turso(
    row: temper_store_postgres::PostgresEvolutionRecordRow,
) -> EvolutionRecordRow {
    EvolutionRecordRow {
        id: row.id,
        record_type: row.record_type,
        status: row.status,
        created_by: row.created_by,
        derived_from: row.derived_from,
        data: row.data,
        timestamp: row.timestamp,
    }
}

fn pg_design_time_event_to_turso(
    row: temper_store_postgres::PostgresDesignTimeEventRow,
) -> DesignTimeEventRow {
    DesignTimeEventRow {
        id: row.id,
        kind: row.kind,
        entity_type: row.entity_type,
        tenant: row.tenant,
        summary: row.summary,
        level: row.level,
        passed: row.passed,
        step_number: row.step_number,
        total_steps: row.total_steps,
        created_at: row.created_at,
    }
}

fn pg_ots_to_turso(row: temper_store_postgres::PostgresOtsTrajectoryRow) -> OtsTrajectoryRow {
    OtsTrajectoryRow {
        trajectory_id: row.trajectory_id,
        tenant: row.tenant,
        agent_id: row.agent_id,
        session_id: row.session_id,
        outcome: row.outcome,
        turn_count: row.turn_count,
        created_at: row.created_at,
    }
}

fn pg_denial_pattern_to_turso(
    row: temper_store_postgres::PostgresPolicyDenialPatternRow,
) -> PolicyDenialPatternRow {
    PolicyDenialPatternRow {
        tenant: row.tenant,
        agent_type: row.agent_type,
        action: row.action,
        resource_type: row.resource_type,
        count: row.count,
        first_seen: row.first_seen,
        last_seen: row.last_seen,
        distinct_resource_ids_json: row.distinct_resource_ids_json,
    }
}

fn pg_wasm_metadata_to_turso(
    row: temper_store_postgres::PostgresWasmModuleMetadataRow,
) -> TursoWasmModuleMetadataRow {
    TursoWasmModuleMetadataRow {
        tenant: row.tenant,
        module_name: row.module_name,
        sha256_hash: row.sha256_hash,
        size_bytes: row.size_bytes,
        updated_at: row.updated_at,
    }
}

fn pg_wasm_invocation_to_turso(
    row: temper_store_postgres::PostgresWasmInvocationRow,
) -> TursoWasmInvocationRow {
    TursoWasmInvocationRow {
        tenant: row.tenant,
        entity_type: row.entity_type,
        entity_id: row.entity_id,
        module_name: row.module_name,
        trigger_action: row.trigger_action,
        callback_action: row.callback_action,
        success: row.success,
        error: row.error,
        duration_ms: row.duration_ms,
        created_at: row.created_at,
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

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        match self {
            Self::Postgres(store) => store.list_entity_ids_by_type(tenant, entity_type).await,
            Self::Turso(store) => store.list_entity_ids_by_type(tenant, entity_type).await,
            Self::Redis(store) => store.list_entity_ids_by_type(tenant, entity_type).await,
            Self::TenantRouted(router) => router.list_entity_ids_by_type(tenant, entity_type).await,
            #[cfg(feature = "sim")]
            Self::Sim(store, _) => store.list_entity_ids_by_type(tenant, entity_type).await,
        }
    }
}
