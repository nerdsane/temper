//! Entity lifecycle methods for ServerState (spawn, query, delete, index).

mod actor_resolution;
#[cfg(test)]
#[path = "entity_ops/actor_resolution_test.rs"]
mod actor_resolution_test;
mod entity_loading;
mod field_updates;
mod generation_access;
mod key_activation;
mod key_index_coverage;
mod status_resolution;

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use sha2::{Digest, Sha256};
use tracing::{Instrument, instrument};

use temper_observe::wide_event;
use temper_runtime::persistence::{
    EventMetadata, IndexReconciliation, PersistenceEnvelope, PersistenceError, SnapshotSourceFence,
};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use super::dispatch::retry;
use super::{ServerState, SpecPublicationGuard, projection_backfill};
use crate::entity_actor::{
    EntityActor, EntityEvent, EntityMsg, EntityPassivationSnapshot, EntityResponse, EntityState,
};
use crate::events::EntityStateChange;
use crate::registry::{VerificationDetail, VerificationStatus};
use crate::request_context::AgentContext;
use crate::runtime_metrics;
use crate::storage::DataOnlyCreateRecord;

fn actor_idle_timeout_secs() -> i64 {
    static ACTOR_IDLE_TIMEOUT: OnceLock<i64> = OnceLock::new();
    *ACTOR_IDLE_TIMEOUT.get_or_init(|| {
        std::env::var("TEMPER_ACTOR_IDLE_TIMEOUT") // determinism-ok: read once at startup
            .ok()
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(300)
    })
}

fn record_projection_update_started(
    tenant: &TenantId,
    entity_type: &str,
    operation: &str,
    source: &str,
) {
    crate::query_projection_metrics::record_update_started(
        tenant.as_str(),
        entity_type,
        operation,
        source,
    );
}

fn record_projection_update_success(
    tenant: &TenantId,
    entity_type: &str,
    operation: &str,
    source: &str,
    sequence_nr: u64,
    started_at: Instant,
) {
    let duration = started_at.elapsed();
    crate::query_projection_metrics::record_update_duration(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        "ok",
        duration,
    );
    crate::query_projection_metrics::record_update_end_to_end_duration(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        "ok",
        duration,
    );
    crate::query_projection_metrics::record_update_applied_sequence(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        sequence_nr,
    );
}

fn record_projection_update_error(
    tenant: &TenantId,
    entity_type: &str,
    operation: &str,
    source: &str,
    started_at: Instant,
) {
    let duration = started_at.elapsed();
    crate::query_projection_metrics::record_update_duration(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        "error",
        duration,
    );
    crate::query_projection_metrics::record_update_end_to_end_duration(
        tenant.as_str(),
        entity_type,
        operation,
        source,
        "error",
        duration,
    );
    crate::query_projection_metrics::record_update_error(
        tenant.as_str(),
        entity_type,
        operation,
        source,
    );
}

/// Error returned when the verification gate blocks an operation.
#[derive(Debug, Clone, serde::Serialize)]
pub struct VerificationGateError {
    /// The entity type that failed the gate.
    pub entity_type: String,
    /// Gate status: "pending", "running", or "failed".
    pub status: String,
    /// Human-readable message.
    pub message: String,
    /// Failed verification levels with details (only for "failed" status).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed_levels: Option<Vec<FailedLevelInfo>>,
}

/// Information about a failed verification level.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FailedLevelInfo {
    /// Level name (e.g. "Level 2: Deterministic Simulation").
    pub level: String,
    /// Human-readable summary of the failure.
    pub summary: String,
    /// Detailed violation information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<Vec<VerificationDetail>>,
}

/// Snapshot of an entity as seen by Cedar authorization.
#[derive(Debug, Clone)]
pub(crate) struct AuthzResourceSnapshot {
    pub(crate) current_state: EntityResponse,
    pub(crate) resource_attrs: BTreeMap<String, serde_json::Value>,
}

/// Fenced key-ownership work prepared before a registry publication. Callers
/// publish tables with [`Self::activation_epochs`], then finish the cutover to
/// evict old actors, publish coverage readiness, and reopen actor spawn.
#[derive(Debug)]
pub struct KeyContractActivationCutover {
    /// Durable activation epoch assigned to each published or removed type.
    pub activation_epochs: BTreeMap<String, u64>,
    candidate_tables: Vec<(String, temper_jit::TransitionTable)>,
    prepared_coverage: Vec<projection_backfill::PreparedKeyIndexCoverage>,
    affected_entity_types: std::collections::BTreeSet<String>,
}

impl KeyContractActivationCutover {
    fn empty() -> Self {
        Self {
            activation_epochs: BTreeMap::new(),
            candidate_tables: Vec::new(),
            prepared_coverage: Vec::new(),
            affected_entity_types: std::collections::BTreeSet::new(),
        }
    }
}

impl ServerState {
    /// Arm a publication immediately before durable persistence. From this
    /// point, cancellation or any publication error intentionally leaves the
    /// tenant fail-closed until a serialized retry completes the cutover: a
    /// failed commit acknowledgement cannot prove that no commit occurred.
    pub fn arm_spec_publication(
        &self,
        publication_guard: &mut SpecPublicationGuard,
        tenant: &TenantId,
        intent_fingerprint: &str,
    ) -> Result<(), String> {
        publication_guard.arm(tenant, intent_fingerprint)?;
        self.evict_tenant_actors(tenant);
        Ok(())
    }

    fn touch_actor_access(&self, actor_key: &str) {
        if let Ok(mut last_accessed) = self.last_accessed.write() {
            last_accessed.insert(actor_key.to_string(), sim_now());
        }
    }

    /// Number of currently active (in-memory) entity actors.
    pub fn active_actor_count(&self) -> u64 {
        self.actor_registry
            .read()
            .map(|registry| registry.len() as u64)
            .unwrap_or(0)
    }

    /// Number of entities currently tracked by the in-memory entity index.
    pub fn active_entity_count(&self) -> u64 {
        self.entity_index
            .read()
            .map(|index| index.values().map(|ids| ids.len() as u64).sum())
            .unwrap_or(0)
    }

    /// Active entity counts grouped by tenant from the in-memory index.
    pub fn active_entity_counts_by_tenant(&self) -> BTreeMap<String, u64> {
        self.entity_index
            .read()
            .map(|index| {
                let mut counts = BTreeMap::new();
                for (index_key, ids) in index.iter() {
                    if let Some((tenant, _entity_type)) = index_key.split_once(':') {
                        *counts.entry(tenant.to_string()).or_insert(0) += ids.len() as u64;
                    }
                }
                counts
            })
            .unwrap_or_default()
    }

    /// Returns `true` when a tenant/entity_type has a registered spec.
    pub(crate) fn has_registered_spec(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<bool, String> {
        self.registry
            .read()
            .map(|registry| registry.get_spec(tenant, entity_type).is_some())
            .map_err(|e| format!("registry lock poisoned: {e}"))
    }

    /// Returns `true` when dispatch should be allowed for the entity type.
    ///
    /// This includes both tenant-scoped specs and legacy single-tenant
    /// transition tables.
    pub(crate) fn is_entity_type_governed(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<bool, String> {
        Ok(self.has_registered_spec(tenant, entity_type)?
            || self.transition_tables.contains_key(entity_type))
    }

    /// Declared `[[key]]` set for a `(tenant, entity_type)` (ADR-0153), resolved
    /// from the SAME sources dispatch uses: the per-tenant registry first — where
    /// runtime-installed os-app entities (File, Directory, SessionEntry, …) live —
    /// then the legacy single-tenant transition tables.
    ///
    /// The keyed read fast path MUST resolve keys through here. Reading
    /// `transition_tables` directly only sees the boot-time single-tenant set and
    /// silently omits every registry-installed entity, disabling the keyed path so
    /// point reads fall back to the budget-bounded scan and 413 at scale — the
    /// TemperFS root-directory failure in ARN-68.
    pub(crate) fn declared_keys_for(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Vec<temper_jit::table::types::DeclaredKey> {
        // Fail fast on a poisoned registry lock rather than silently falling through
        // to `transition_tables` — a silent fallback would re-introduce exactly the
        // ARN-68 bug (registry-installed keys not found → keyed path disabled → scan).
        {
            let registry = self.registry.read().expect("registry lock poisoned");
            if let Some(table) = registry.get_table(tenant, entity_type) {
                return table.keys.clone();
            }
        }
        self.transition_tables
            .get(entity_type)
            .map(|table| table.keys.clone())
            .unwrap_or_default()
    }

    /// The declared `[[vector]]` access paths for `(tenant, entity_type)` — the
    /// registry table first (covers os-app entities), the boot-time
    /// `transition_tables` as fallback (ADR-0155). Same registry-lock discipline as
    /// [`Self::declared_keys_for`]: fail fast on a poisoned lock rather than silently
    /// falling through. Empty when the type declares no vector path.
    pub(crate) fn declared_vectors_for(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Vec<temper_jit::table::types::DeclaredVector> {
        {
            let registry = self.registry.read().expect("registry lock poisoned");
            if let Some(table) = registry.get_table(tenant, entity_type) {
                return table.vectors.clone();
            }
        }
        self.transition_tables
            .get(entity_type)
            .map(|table| table.vectors.clone())
            .unwrap_or_default()
    }

    /// Load the current entity state and derive the Cedar resource view used
    /// for action authorization.
    pub(crate) async fn load_authz_resource_snapshot(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<AuthzResourceSnapshot, String> {
        let current_state = self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await?;

        let mut resource_attrs = BTreeMap::new();
        resource_attrs.insert(
            "id".to_string(),
            serde_json::Value::String(entity_id.to_string()),
        );
        resource_attrs.insert(
            "status".to_string(),
            serde_json::Value::String(current_state.state.status.clone()),
        );
        if let serde_json::Value::Object(fields) = &current_state.state.fields {
            for (k, v) in fields {
                resource_attrs.insert(k.clone(), v.clone());
            }
        }

        let context_entities: Vec<temper_spec::automaton::ContextEntityDecl> = self
            .registry
            .read()
            .map_err(|e| format!("registry lock poisoned: {e}"))?
            .get_spec(tenant, entity_type)
            .map(|s| s.automaton.context_entities.clone())
            .unwrap_or_default();

        for ce in &context_entities {
            let target_id = current_state
                .state
                .fields
                .get(&ce.id_field)
                .and_then(|v| v.as_str())
                .unwrap_or("");

            if !target_id.is_empty()
                && let Some(status) = self
                    .resolve_entity_status(tenant, &ce.entity_type, target_id)
                    .await
            {
                resource_attrs.insert(
                    format!("ctx_{}_status", ce.name),
                    serde_json::Value::String(status),
                );
            }
        }

        let has_spec = self.has_registered_spec(tenant, entity_type)?;
        resource_attrs.insert("has_spec".to_string(), serde_json::Value::Bool(has_spec));

        Ok(AuthzResourceSnapshot {
            current_state,
            resource_attrs,
        })
    }

    /// Mark every entity type observed in `entities` as fully hydrated from the
    /// durable store, so `list_entity_ids_lazy` serves it from the in-memory
    /// index without a redundant store scan. Used by the paths that load the
    /// authoritative set for a tenant (full and eager hydration).
    fn mark_types_hydrated(&self, tenant: &TenantId, entities: &[(String, String)]) {
        let keys: std::collections::BTreeSet<String> = entities
            .iter()
            .map(|(entity_type, _entity_id)| format!("{tenant}:{entity_type}"))
            .collect();
        if keys.is_empty() {
            return;
        }
        self.entity_index_hydrated
            .write()
            .expect("entity index hydrated lock poisoned")
            .extend(keys);
    }

    /// Populate `entity_index` from the event store without spawning actors.
    ///
    /// This is the memory-safe startup/list path: we discover persisted
    /// entities while deferring actor allocation until first access.
    #[instrument(skip_all, fields(otel.name = "entity.populate_index_from_store", tenant = %tenant))]
    pub async fn populate_index_from_store(&self, tenant: &TenantId) {
        let Some((store, _backend)) = self.event_journal() else {
            return;
        };

        match store.list_entity_ids(tenant.as_str()).await {
            Ok(entities) => {
                {
                    let mut index = self
                        .entity_index
                        .write()
                        .expect("entity index lock poisoned");
                    for (entity_type, entity_id) in &entities {
                        let index_key = format!("{tenant}:{entity_type}");
                        index
                            .entry(index_key)
                            .or_default()
                            .insert(entity_id.clone());
                    }
                } // write lock dropped before metrics call
                // A full-store scan is authoritative for every type it observed.
                self.mark_types_hydrated(tenant, &entities);
                tracing::info!(
                    tenant = %tenant,
                    count = entities.len(),
                    "populated entity index from event store"
                );
                runtime_metrics::record_server_state_metrics(self);
            }
            Err(e) => {
                tracing::error!(
                    tenant = %tenant,
                    error = %e,
                    "failed to populate entity index from event store"
                );
            }
        }
    }

    /// Populate `entity_index` for one entity type from the event store.
    #[instrument(skip_all, fields(
        otel.name = "entity.populate_index_from_store_by_type",
        tenant = %tenant,
        entity_type,
    ))]
    pub async fn populate_index_from_store_by_type(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> usize {
        let Some((store, _backend)) = self.event_journal() else {
            return 0;
        };

        match store
            .list_entity_ids_by_type(tenant.as_str(), entity_type)
            .await
        {
            Ok(entity_ids) => {
                let count = entity_ids.len();
                {
                    let mut index = self
                        .entity_index
                        .write()
                        .expect("entity index lock poisoned");
                    let index_key = format!("{tenant}:{entity_type}");
                    let ids = index.entry(index_key).or_default();
                    for entity_id in entity_ids {
                        ids.insert(entity_id);
                    }
                }
                // The store scan is authoritative for this type: mark it fully
                // hydrated so `list_entity_ids_lazy` can serve from the index.
                self.entity_index_hydrated
                    .write()
                    .expect("entity index hydrated lock poisoned")
                    .insert(format!("{tenant}:{entity_type}"));
                tracing::info!(
                    tenant = %tenant,
                    entity_type,
                    count,
                    "populated typed entity index from event store"
                );
                runtime_metrics::record_server_state_metrics(self);
                count
            }
            Err(e) => {
                tracing::error!(
                    tenant = %tenant,
                    entity_type,
                    error = %e,
                    "failed to populate typed entity index from event store"
                );
                0
            }
        }
    }

    /// ADR-0155: backfill `entity_vector_index` for pre-existing entities of every
    /// vector-declaring type and record the watermark. Idempotent; entities written
    /// after boot maintain their vectors inline (co-commit) or write-behind.
    #[instrument(skip_all, fields(otel.name = "entity.populate_vector_index", tenant = %tenant))]
    pub async fn populate_vector_index_from_snapshots(&self, tenant: &TenantId) {
        projection_backfill::populate_vector_index_from_snapshots(self, tenant).await;
    }

    /// Compare durable projection rows with authoritative state rebuilt by event replay.
    pub async fn verify_query_projection_replay_parity(
        &self,
        tenant: &TenantId,
    ) -> Result<super::QueryProjectionReplayParityReport, String> {
        projection_backfill::verify_query_projection_replay_parity(
            self,
            tenant,
            None,
            None,
            "manual_full",
        )
        .await
    }

    /// Compare a bounded projection scope with authoritative event replay.
    pub async fn verify_query_projection_replay_parity_bounded(
        &self,
        tenant: &TenantId,
        entity_type: Option<&str>,
        entity_limit: Option<usize>,
        source: &str,
    ) -> Result<super::QueryProjectionReplayParityReport, String> {
        projection_backfill::verify_query_projection_replay_parity(
            self,
            tenant,
            entity_type,
            entity_limit,
            source,
        )
        .await
    }

    /// Hydrate actor state from the event store by spawning actors for all
    /// entities that have persisted events in this tenant.
    #[instrument(skip_all, fields(otel.name = "entity.hydrate_from_store", tenant = %tenant))]
    pub async fn hydrate_from_store(&self, tenant: &TenantId) {
        if let Some((store, _backend)) = self.event_journal() {
            match store.list_entity_ids(tenant.as_str()).await {
                Ok(entities) => {
                    let mut hydrated = 0usize;
                    for (entity_type, entity_id) in &entities {
                        if self
                            .ensure_entity_loaded(tenant, entity_type, entity_id)
                            .await
                        {
                            hydrated = hydrated.saturating_add(1);
                        }
                    }
                    // Eager hydration loaded every durable entity, so each
                    // observed type's index is complete — mark it hydrated so
                    // the first lazy list does not re-scan the store.
                    self.mark_types_hydrated(tenant, &entities);
                    tracing::info!(
                        tenant = %tenant,
                        count = hydrated,
                        discovered = entities.len(),
                        "hydrated entities from event store"
                    );
                    runtime_metrics::record_server_state_metrics(self);
                }
                Err(e) => {
                    tracing::error!(
                        tenant = %tenant,
                        error = %e,
                        "failed to hydrate from event store"
                    );
                }
            }
        }
    }

    /// Stop and evict an entity actor plus its in-memory indexes.
    ///
    /// Used after an out-of-band durable append (for example, an atomic
    /// Composite batch) so the next read hydrates from the authoritative event
    /// journal instead of serving stale actor state.
    #[instrument(skip_all, fields(otel.name = "entity.stop_and_remove_entity", tenant = %tenant, entity_type, entity_id))]
    pub(crate) fn stop_and_remove_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) {
        let actor_key = format!("{tenant}:{entity_type}:{entity_id}");

        if let Ok(mut registry) = self.actor_registry.write()
            && let Some(actor_ref) = registry.remove(&actor_key)
        {
            let _ = actor_ref.stop();
        }
        if let Ok(mut last_accessed) = self.last_accessed.write() {
            last_accessed.remove(&actor_key);
        }
        if let Ok(mut index) = self.entity_index.write() {
            let index_key = format!("{tenant}:{entity_type}");
            if let Some(ids) = index.get_mut(&index_key) {
                ids.remove(entity_id);
            }
        }
        runtime_metrics::record_server_state_metrics(self);
    }

    /// List all entity IDs for a (tenant, entity_type) pair.
    #[instrument(skip_all, fields(otel.name = "entity.list_entity_ids", tenant = %tenant, entity_type))]
    pub fn list_entity_ids(&self, tenant: &TenantId, entity_type: &str) -> Vec<String> {
        let index_key = format!("{tenant}:{entity_type}");
        let index = self.entity_index.read().unwrap();
        index
            .get(&index_key)
            .map(|ids| ids.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Check authorization for an action using the Cedar ABAC engine.
    ///
    /// Returns a typed [`AuthzDenial`] on failure, preserving the denial kind
    /// (policy denied, no matching permit, invalid principal, etc.).
    ///
    /// Accepts `BTreeMap` for DST compliance; converts at the authz boundary.
    pub fn authorize(
        &self,
        headers: &[(String, String)],
        action: &str,
        resource_type: &str,
        resource_attrs: &BTreeMap<String, serde_json::Value>,
    ) -> Result<(), AuthzDenial> {
        let ctx = SecurityContext::from_headers(headers);
        self.authorize_with_context(&ctx, action, resource_type, resource_attrs, "default")
    }

    /// Check authorization using a pre-built `SecurityContext`.
    ///
    /// Unlike [`authorize`] which builds the context from raw headers, this
    /// method accepts an already-constructed context enriched with agent
    /// identity and resource attributes.
    ///
    /// Returns a typed [`AuthzDenial`] on failure, preserving the denial kind.
    ///
    /// Accepts `BTreeMap` for DST compliance; converts at the authz boundary.
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(otel.name = "entity.authorize_with_context", action, resource_type))]
    pub fn authorize_with_context(
        &self,
        security_ctx: &SecurityContext,
        action: &str,
        resource_type: &str,
        resource_attrs: &BTreeMap<String, serde_json::Value>,
        tenant: &str,
    ) -> Result<(), AuthzDenial> {
        let attrs: std::collections::HashMap<_, _> = resource_attrs // determinism-ok: Cedar API requires HashMap
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(); // determinism-ok
        let authz_start = sim_now();
        let decision = self.authz.authorize_for_tenant_or_bypass(
            tenant,
            security_ctx,
            action,
            resource_type,
            &attrs,
        );
        let duration_ns = (sim_now() - authz_start)
            .num_nanoseconds()
            .unwrap_or(0)
            .max(0) as u64;
        let decision_str = match &decision {
            AuthzDecision::Allow { .. } => "Allow",
            AuthzDecision::Deny(_) => "Deny",
        };
        let wide = wide_event::from_authz_decision(wide_event::AuthzDecisionInput {
            action,
            resource_type,
            principal_kind: &format!("{:?}", security_ctx.principal.kind),
            decision: decision_str,
            duration_ns,
            tenant,
        });
        wide_event::emit_span(&wide);
        wide_event::emit_metrics(&wide);
        match decision {
            AuthzDecision::Allow { .. } => Ok(()),
            AuthzDecision::Deny(denial) => Err(denial),
        }
    }

    /// Create a durable data-only entity without spawning an actor.
    ///
    /// This path is only eligible for transition tables with no rules. It
    /// preserves the event journal, projection acknowledgement, in-memory
    /// entity index, and observe/SSE event contracts used by the actor path.
    #[instrument(skip_all, fields(otel.name = "entity.create_data_only_tenant_entity_fast_path", tenant = %tenant, entity_type, entity_id))]
    pub async fn try_create_data_only_tenant_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
    ) -> Result<Option<EntityResponse>, String> {
        if self.key_contract_activation_gated(tenant, entity_type) {
            return Ok(None);
        }
        if !initial_fields.is_object() {
            return Ok(None);
        }
        if self.is_pg_actor_backed(tenant, entity_type) {
            return Ok(None);
        }

        let table = {
            let reg = self.registry.read().unwrap();
            reg.get_table_live(tenant, entity_type)
        }
        .or_else(|| {
            self.transition_tables
                .get(entity_type)
                .map(|t| Arc::new(RwLock::new((**t).clone())))
        });
        let Some(table_ref) = table else {
            return Ok(None);
        };
        let table = table_ref
            .read()
            .expect("transition table lock poisoned")
            .clone();
        if !table.rules.is_empty() || !table.vectors.is_empty() {
            return Ok(None);
        }

        let Some((store, backend)) = self.event_journal() else {
            return Ok(None);
        };
        let Some(query_plane) = self.query_plane_store() else {
            return Ok(None);
        };

        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let mut fields = initial_fields.clone();
        if let Some(obj) = fields.as_object_mut() {
            obj.entry("Id".to_string())
                .or_insert(serde_json::Value::String(entity_id.to_string()));
            obj.entry("Status".to_string())
                .or_insert(serde_json::Value::String(table.initial_state.clone()));
        }

        let mut state = EntityState {
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            status: table.initial_state.clone(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields,
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: BTreeMap::new(),
        };

        let created = EntityEvent {
            action: "Created".to_string(),
            from_status: String::new(),
            to_status: state.status.clone(),
            timestamp: sim_now(),
            params: initial_fields,
            idempotency_key: None,
        };
        let payload = serde_json::to_value(&created)
            .map_err(|e| format!("failed to serialize Created event: {e}"))?;
        let envelope = PersistenceEnvelope {
            sequence_nr: 1,
            event_type: created.action.clone(),
            payload,
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: created.timestamp,
                actor_id: persistence_id.clone(),
            },
        };

        let projection_fields = self.query_projection_fields(tenant, entity_type, &state.fields);
        let mut created_projection_state = state.clone();
        created_projection_state.sequence_nr = 1;
        created_projection_state.push_event_bounded(created.clone());
        let projection_state = self.query_projection_state(&created_projection_state);
        // The empty declared-key contract is authoritative too: it releases
        // ownership rows from a previously keyed version of this entity type.
        let reconcile_keys = true;
        let key_set_signature = crate::key_index::declared_key_write_contract(&table);
        let key_rows = crate::key_index::derive_entity_key_rows(&table.keys, &state.fields, true);
        if let Some(native_store) = self.data_only_create_store() {
            let operation = "native_create";
            let source = "data_only_create_fast_path";
            record_projection_update_started(tenant, entity_type, operation, source);
            let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
            let native_span = tracing::info_span!(
                "entity.data_only_create_native_storage",
                otel.name = "entity.data_only_create_native_storage",
                tenant = %tenant,
                entity_type,
                entity_id,
            );
            match native_store
                .create_data_only_entity(DataOnlyCreateRecord {
                    tenant: tenant.as_str(),
                    entity_type,
                    entity_id,
                    status: &state.status,
                    fields: &projection_fields,
                    state: &projection_state,
                    event: &envelope,
                    key_rows: &key_rows,
                    reconcile_keys,
                    key_set_signature: &key_set_signature,
                })
                .instrument(native_span)
                .await
            {
                Ok(new_seq) => {
                    state.sequence_nr = new_seq;
                    record_projection_update_success(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        state.sequence_nr,
                        projection_started_at,
                    );
                }
                Err(
                    PersistenceError::ConcurrencyViolation { .. }
                    | PersistenceError::SnapshotGenerationChanged,
                ) => {
                    record_projection_update_error(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        projection_started_at,
                    );
                    return Ok(None);
                }
                Err(e) => {
                    record_projection_update_error(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        projection_started_at,
                    );
                    tracing::error!(
                        error = %e,
                        tenant = %tenant,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        "failed to update query projection during native data-only create fast path"
                    );
                    return Err(format!(
                        "native data-only create failed for {entity_type}:{entity_id}: {e}"
                    ));
                }
            }
        } else {
            let append_started_at = Instant::now(); // determinism-ok: production-only append wait metric
            let append_result = store
                .append_with_index_rows(
                    &persistence_id,
                    state.sequence_nr,
                    &[envelope],
                    &key_rows,
                    &[],
                    IndexReconciliation {
                        keys: reconcile_keys,
                        key_set_signature: Some(key_set_signature),
                        vectors: false,
                        snapshot_source: SnapshotSourceFence::Absent,
                    },
                )
                .await;
            runtime_metrics::record_event_store_append_wait(
                backend.as_str(),
                if reconcile_keys {
                    "append_with_keys"
                } else {
                    "append"
                },
                append_started_at.elapsed(),
            );
            match append_result {
                Ok(new_seq) => {
                    state.sequence_nr = new_seq;
                }
                Err(
                    PersistenceError::ConcurrencyViolation { .. }
                    | PersistenceError::SnapshotGenerationChanged,
                ) => {
                    return Ok(None);
                }
                Err(e) => {
                    return Err(format!(
                        "failed to persist data-only Created event for {entity_type}:{entity_id}: {e}"
                    ));
                }
            }

            let operation = "upsert";
            let source = "data_only_create_fast_path";
            record_projection_update_started(tenant, entity_type, operation, source);
            let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
            if let Err(e) = query_plane
                .upsert_projection(
                    tenant.as_str(),
                    entity_type,
                    entity_id,
                    &state.status,
                    &projection_fields,
                    &projection_state,
                    state.sequence_nr,
                )
                .await
            {
                record_projection_update_error(
                    tenant,
                    entity_type,
                    operation,
                    source,
                    projection_started_at,
                );
                tracing::error!(
                    error = %e,
                    tenant = %tenant,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "failed to update query projection during data-only create fast path"
                );
                return Err(format!(
                    "query projection write failed during data-only create: {e}"
                ));
            }
            record_projection_update_success(
                tenant,
                entity_type,
                operation,
                source,
                state.sequence_nr,
                projection_started_at,
            );
        }
        state.push_event_bounded(created);

        {
            let index_key = format!("{tenant}:{entity_type}");
            let mut index = self.entity_index.write().unwrap();
            index
                .entry(index_key)
                .or_default()
                .insert(entity_id.to_string());
        }
        runtime_metrics::record_server_state_metrics(self);

        let seq = self.next_entity_event_sequence(tenant.as_str(), entity_type, entity_id);
        let change = EntityStateChange {
            seq,
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            action: "Created".to_string(),
            status: state.status.clone(),
            tenant: tenant.to_string(),
            agent_id: None,
            session_id: None,
            intent: None,
            observation_metadata: None,
        };
        self.record_entity_observe_event_with_seq(
            tenant.as_str(),
            entity_type,
            entity_id,
            seq,
            "state_change",
            serde_json::to_value(&change).unwrap_or_default(),
        );
        let _ = self.event_tx.send(change);

        Ok(Some(EntityResponse {
            success: true,
            state,
            error: None,
            custom_effects: vec![],
            scheduled_actions: vec![],
            spawn_requests: vec![],
            spec_governed: true,
        }))
    }

    /// Delete an entity.
    #[instrument(skip_all, fields(otel.name = "entity.delete_tenant_entity", tenant = %tenant, entity_type, entity_id))]
    pub async fn delete_tenant_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<EntityResponse, String> {
        let actor_ref = self
            .get_or_spawn_tenant_actor(tenant, entity_type, entity_id)
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;

        let policy = self.dispatch_retry_policy();
        let response = retry::ask_with_backoff::<_, EntityResponse, _>(
            &actor_ref,
            || EntityMsg::Delete,
            &policy,
        )
        .await
        .result
        .map_err(|e| format!("Actor delete failed: {e}"))?;

        if response.success {
            if let Some(query_plane) = self.query_plane_store() {
                let operation = "remove";
                let source = "delete";
                record_projection_update_started(tenant, entity_type, operation, source);
                let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
                if let Err(e) = query_plane
                    .remove_projection(tenant.as_str(), entity_type, entity_id)
                    .await
                {
                    record_projection_update_error(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        projection_started_at,
                    );
                    // Delete is idempotent against the projection: a stale row
                    // surviving in entity_field_index after the tombstone is a
                    // visibility leak, not data corruption. Log loudly so it's
                    // greppable but don't block the delete from completing —
                    // the actor and event journal already reflect the tombstone.
                    tracing::error!(
                        error = %e,
                        tenant = %tenant,
                        entity_type = %entity_type,
                        entity_id = %entity_id,
                        "failed to remove query projection during delete (stale projection row will linger until next successful upsert/delete)"
                    );
                } else {
                    record_projection_update_success(
                        tenant,
                        entity_type,
                        operation,
                        source,
                        response.state.sequence_nr,
                        projection_started_at,
                    );
                }
            }

            // Tombstone persisted successfully; now it is safe to remove actor
            // and in-memory index entries.
            let _ = actor_ref.stop();
            self.remove_entity(tenant, entity_type, entity_id);
        }

        Ok(response)
    }

    /// Check if an entity exists in the index.
    pub fn entity_exists(&self, tenant: &TenantId, entity_type: &str, entity_id: &str) -> bool {
        let index_key = format!("{tenant}:{entity_type}");
        let index = self.entity_index.read().unwrap();
        index
            .get(&index_key)
            .is_some_and(|ids| ids.contains(entity_id))
    }

    /// List entity IDs for a type, guaranteeing completeness against the
    /// durable event store.
    ///
    /// The in-memory index is served only once the type has been fully
    /// hydrated from the store. A non-empty index is NOT sufficient: lazily
    /// spawning a single actor inserts just that id, so trusting "non-empty
    /// means complete" lets a partial index hide durable entities, and a
    /// collection query then silently returns a partial set. When the type is
    /// not yet hydrated we scan the store once (which marks it complete) and
    /// then serve from the index on subsequent calls.
    #[instrument(skip_all, fields(otel.name = "entity.list_entity_ids_lazy", tenant = %tenant, entity_type))]
    pub async fn list_entity_ids_lazy(&self, tenant: &TenantId, entity_type: &str) -> Vec<String> {
        let index_key = format!("{tenant}:{entity_type}");
        let already_hydrated = self
            .entity_index_hydrated
            .read()
            .expect("entity index hydrated lock poisoned")
            .contains(&index_key);
        if already_hydrated {
            return self.list_entity_ids(tenant, entity_type);
        }

        // No durable journal to reconcile against: the in-memory index is all
        // there is, so return it as-is.
        if self.event_journal().is_none() {
            return self.list_entity_ids(tenant, entity_type);
        }

        self.populate_index_from_store_by_type(tenant, entity_type)
            .await;
        self.list_entity_ids(tenant, entity_type)
    }

    /// Passivate actors that have been idle longer than the configured timeout.
    ///
    /// Keeps `entity_index` entries intact so future accesses can lazy-spawn.
    #[instrument(skip_all, fields(otel.name = "entity.passivate_idle_actors"))]
    pub async fn passivate_idle_actors(&self) {
        let timeout_secs = actor_idle_timeout_secs();
        let cutoff = sim_now() - chrono::Duration::seconds(timeout_secs);

        let candidates: Vec<(String, ActorRef<EntityMsg>)> = {
            let Ok(registry) = self.actor_registry.read() else {
                return;
            };
            let Ok(last_accessed) = self.last_accessed.read() else {
                return;
            };
            registry
                .iter()
                .filter_map(|(key, actor_ref)| {
                    let last_seen = last_accessed.get(key)?;
                    if *last_seen <= cutoff {
                        Some((key.clone(), actor_ref.clone()))
                    } else {
                        None
                    }
                })
                .collect()
        };

        if candidates.is_empty() {
            return;
        }

        let mut passivated = 0usize;
        let policy = self.dispatch_retry_policy();
        let journal = self.event_journal();
        for (actor_key, actor_ref) in candidates {
            if temper_runtime::tenant::parse_persistence_id_parts(&actor_key)
                .ok()
                .is_some_and(|(tenant, entity_type, _)| {
                    self.key_contract_activation_gated(&TenantId::new(tenant), entity_type)
                })
            {
                continue;
            }
            // ADR-0048: retry transient failures so passivation is not skipped
            // by a single AskTimeout under load.
            let snapshot_outcome = retry::ask_with_backoff::<_, EntityPassivationSnapshot, _>(
                &actor_ref,
                || EntityMsg::GetPassivationSnapshot,
                &policy,
            )
            .await;
            if let Some((store, _backend)) = journal.as_ref()
                && let Ok(response) = snapshot_outcome.result
                && response.state.sequence_nr > 0
            {
                let journal_provenance = match store.journal_boundary(&actor_key).await {
                    Ok(boundary) if boundary.latest_sequence == 0 => Some(None),
                    Ok(boundary) if boundary.latest_sequence == response.state.sequence_nr => {
                        Some(Some(boundary.latest_sequence))
                    }
                    Ok(boundary) => {
                        tracing::warn!(
                            actor_key = %actor_key,
                            actor_sequence = response.state.sequence_nr,
                            journal_sequence = boundary.latest_sequence,
                            "skipping passivation snapshot because actor and journal differ"
                        );
                        None
                    }
                    Err(error) => {
                        tracing::warn!(
                            actor_key = %actor_key,
                            error = %error,
                            "skipping passivation snapshot because journal provenance is unavailable"
                        );
                        None
                    }
                };
                if let Some(journal_sequence) = journal_provenance {
                    match EntityActor::serialize_snapshot_state(&response.state, journal_sequence) {
                        Ok(snapshot_bytes) => {
                            if let Err(e) = store
                                .save_snapshot_if_source(
                                    &actor_key,
                                    response.state.sequence_nr,
                                    &snapshot_bytes,
                                    &response.snapshot_source,
                                    Some(&response.key_contract),
                                )
                                .await
                            {
                                tracing::warn!(
                                    actor_key = %actor_key,
                                    seq = response.state.sequence_nr,
                                    error = %e,
                                    "failed to save snapshot during passivation"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                actor_key = %actor_key,
                                error = %e,
                                "failed to encode snapshot during passivation"
                            );
                        }
                    }
                }
            }

            let _ = actor_ref.stop();

            let removed = {
                let Ok(mut registry) = self.actor_registry.write() else {
                    continue;
                };
                if registry
                    .get(&actor_key)
                    .is_some_and(|current| current.id().uid == actor_ref.id().uid)
                {
                    registry.remove(&actor_key);
                    true
                } else {
                    false
                }
            };

            if removed {
                if let Ok(mut last_accessed) = self.last_accessed.write() {
                    last_accessed.remove(&actor_key);
                }
                // Evict the state cache entry so stale status doesn't linger.
                if let Ok(mut cache) = self.entity_state_cache.lock() {
                    cache.pop(&actor_key);
                }
                passivated += 1;
            }
        }

        if passivated > 0 {
            runtime_metrics::record_server_state_metrics(self);
            tracing::info!(count = passivated, timeout_secs, "passivated idle actors");
        }
    }

    /// Update Agent.Hint annotations based on trajectory analysis.
    pub fn enrich_metadata(&self, action_name: &str, hint: &str) {
        const AGENT_HINTS_BUDGET: usize = 1_000;
        let Ok(mut hints) = self.agent_hints.write() else {
            return;
        };
        hints.insert(action_name.to_string(), hint.to_string());
        while hints.len() > AGENT_HINTS_BUDGET {
            let oldest_key = hints.iter().next().map(|(k, _)| k.clone());
            if let Some(k) = oldest_key {
                hints.remove(&k);
            } else {
                break;
            }
        }
    }

    /// Check the verification gate for a specific entity type.
    ///
    /// Returns `Ok(())` if the entity type is verified and operations are allowed.
    /// Returns `Err(VerificationGateError)` if operations should be blocked.
    ///
    /// Policy:
    /// - `None` → `Ok(())` (backward compat for legacy single-tenant without registry)
    /// - `Pending` → `Err("pending")` — verification hasn't started yet
    /// - `Running` → `Err("running")` — verification is in progress
    /// - `Completed(all_passed: true)` → `Ok(())`
    /// - `Completed(all_passed: false)` → `Err("failed")` with failed level details
    #[instrument(skip_all, fields(otel.name = "entity.check_verification_gate", tenant = %tenant, entity_type))]
    pub fn check_verification_gate(
        &self,
        tenant: &TenantId,
        entity_type: &str,
    ) -> Result<(), VerificationGateError> {
        let registry = self.registry.read().unwrap();

        // If there's no tenant config in the registry, this is a legacy
        // single-tenant setup — allow operations for backward compatibility.
        let Some(tenant_config) = registry.get_tenant(tenant) else {
            return Ok(());
        };

        // If the entity type doesn't exist in the tenant, there's nothing to gate.
        if !tenant_config.entities.contains_key(entity_type) {
            return Ok(());
        }

        match tenant_config.verification.get(entity_type) {
            None => Ok(()),
            Some(VerificationStatus::Pending) => Err(VerificationGateError {
                entity_type: entity_type.to_string(),
                status: "pending".to_string(),
                message: format!(
                    "Verification has not started for entity type '{entity_type}'. \
                     Waiting for verification cascade to begin."
                ),
                failed_levels: None,
            }),
            Some(VerificationStatus::Running) => Err(VerificationGateError {
                entity_type: entity_type.to_string(),
                status: "running".to_string(),
                message: format!(
                    "Verification is currently running for entity type '{entity_type}'. \
                     Please wait for the cascade to complete."
                ),
                failed_levels: None,
            }),
            Some(VerificationStatus::Completed(result) | VerificationStatus::Restored(result)) => {
                if result.all_passed {
                    Ok(())
                } else {
                    let failed_levels: Vec<FailedLevelInfo> = result
                        .levels
                        .iter()
                        .filter(|l| !l.passed)
                        .map(|l| FailedLevelInfo {
                            level: l.level.clone(),
                            summary: l.summary.clone(),
                            details: l.details.clone(),
                        })
                        .collect();
                    Err(VerificationGateError {
                        entity_type: entity_type.to_string(),
                        status: "failed".to_string(),
                        message: format!(
                            "Verification failed for entity type '{entity_type}'. \
                             Fix the spec and re-push."
                        ),
                        failed_levels: Some(failed_levels),
                    })
                }
            }
        }
    }
}

use temper_authz::{AuthzDecision, AuthzDenial, SecurityContext};
use temper_runtime::actor::ActorRef;
