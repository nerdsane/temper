//! Metadata and trajectory accessors on ServerState.

use std::sync::Arc;

use crate::storage::MetadataStore;

use super::{ServerState, TrajectoryEntry, TrajectorySource};

impl ServerState {
    /// Find an entity spec by name across all tenants.
    ///
    /// Returns the owning tenant and the IOA source string on success.
    /// Acquires a read lock on the spec registry.
    pub fn find_entity_ioa_source(
        &self,
        entity: &str,
    ) -> Option<(temper_runtime::tenant::TenantId, String)> {
        let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
        for tenant_id in registry.tenant_ids() {
            if let Some(entity_spec) = registry.get_spec(tenant_id, entity) {
                return Some((tenant_id.clone(), entity_spec.ioa_source.clone()));
            }
        }
        None
    }

    /// Load aggregated unmet-intent failure groups for one tenant.
    pub async fn load_unmet_intent_rows_aggregated(
        &self,
        tenant: &str,
    ) -> (
        Vec<temper_store_turso::UnmetIntentAggRow>,
        std::collections::BTreeMap<String, String>,
    ) {
        let Some(store) = self.metadata_store_for_tenant(tenant).await else {
            return (Vec::new(), std::collections::BTreeMap::new());
        };

        let mut failures = Vec::new();
        let mut submitted_specs = std::collections::BTreeMap::new();

        match store.load_unmet_intent_rows(tenant).await {
            Ok(rows) => failures.extend(rows),
            Err(e) => {
                tracing::warn!(error = %e, backend = store.backend_name(), tenant, "failed to load unmet intent rows");
            }
        }
        match store.load_submit_spec_timestamps(tenant).await {
            Ok(map) => submitted_specs.extend(map),
            Err(e) => {
                tracing::warn!(error = %e, backend = store.backend_name(), tenant, "failed to load submit-spec timestamps");
            }
        }
        (failures, submitted_specs)
    }

    /// Count trajectory rows per tenant using fan-out across all metadata stores.
    ///
    /// Returns an empty map when Turso is not configured.
    pub async fn count_trajectories_by_tenant(&self) -> std::collections::BTreeMap<String, u64> {
        let stores = self.collect_all_metadata_stores().await;
        if stores.is_empty() {
            return std::collections::BTreeMap::new();
        }

        let mut counts = std::collections::BTreeMap::new();
        for store in &stores {
            match store.count_trajectories_by_tenant().await {
                Ok(c) => {
                    for (tenant, count) in c {
                        *counts.entry(tenant).or_insert(0) += count;
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, backend = store.backend_name(), "failed to count trajectories by tenant");
                }
            }
        }
        counts
    }

    /// Load trajectory entries owned by one tenant.
    pub async fn load_trajectory_entries(&self, tenant: &str, limit: i64) -> Vec<TrajectoryEntry> {
        let Some(store) = self.metadata_store_for_tenant(tenant).await else {
            return Vec::new();
        };

        let mut all_entries = Vec::new();
        match store.load_recent_trajectories(tenant, limit).await {
            Ok(rows) => {
                all_entries.extend(rows.into_iter().map(|r| TrajectoryEntry {
                    timestamp: r.created_at,
                    tenant: r.tenant,
                    entity_type: r.entity_type,
                    entity_id: r.entity_id,
                    action: r.action,
                    success: r.success,
                    from_status: r.from_status,
                    to_status: r.to_status,
                    error: r.error,
                    agent_id: r.agent_id,
                    session_id: r.session_id,
                    authz_denied: r.authz_denied,
                    denied_resource: r.denied_resource,
                    denied_module: r.denied_module,
                    source: r.source.as_deref().and_then(|s| match s {
                        "Entity" => Some(TrajectorySource::Entity),
                        "Platform" => Some(TrajectorySource::Platform),
                        "Authz" => Some(TrajectorySource::Authz),
                        _ => None,
                    }),
                    spec_governed: r.spec_governed,
                    agent_type: None,
                    request_body: r.request_body.and_then(|s| serde_json::from_str(&s).ok()),
                    intent: r.intent,
                    matched_policy_ids: r.matched_policy_ids,
                    capture_seq: r.capture_seq,
                }));
            }
            Err(e) => {
                tracing::warn!(error = %e, backend = store.backend_name(), tenant, "failed to load trajectories");
            }
        }
        // Sort by timestamp descending and limit
        all_entries.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
        all_entries.truncate(limit as usize);
        all_entries
    }

    /// Collect all Turso stores for fan-out reads.
    ///
    /// In single-DB mode, returns just the shared store.
    /// In TenantRouted mode, returns the platform store + all connected tenant stores.
    /// Returns an empty vec when Turso is not configured.
    pub async fn all_turso_stores(&self) -> Vec<temper_store_turso::TursoEventStore> {
        let Some(provider) = self
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.turso.clone())
        else {
            return Vec::new();
        };
        provider.all_stores().await
    }

    /// Collect backend-neutral platform/tenant stores for cross-tenant reads.
    pub async fn collect_all_metadata_stores(&self) -> Vec<Arc<dyn MetadataStore>> {
        let Some(provider) = self
            .storage_stack
            .as_ref()
            .and_then(|stack| stack.metadata.clone())
        else {
            return Vec::new();
        };
        provider.all_stores().await
    }
}
