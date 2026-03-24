//! Entity lifecycle methods for ServerState (spawn, query, delete, index).

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock, RwLock};

use tracing::instrument;

use temper_observe::wide_event;
use temper_runtime::persistence::{EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use super::ServerState;
use crate::entity_actor::{EntityActor, EntityMsg, EntityResponse};
use crate::events::EntityStateChange;
use crate::registry::{VerificationDetail, VerificationStatus};
use crate::runtime_metrics;

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

fn is_deleted_envelope(event: &PersistenceEnvelope) -> bool {
    if event.event_type == "Deleted" {
        return true;
    }
    event
        .payload
        .get("action")
        .and_then(serde_json::Value::as_str)
        == Some("Deleted")
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

impl ServerState {
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

    /// Populate `entity_index` from the event store without spawning actors.
    ///
    /// This is the memory-safe startup/list path: we discover persisted
    /// entities while deferring actor allocation until first access.
    #[instrument(skip_all, fields(otel.name = "entity.populate_index_from_store", tenant = %tenant))]
    pub async fn populate_index_from_store(&self, tenant: &TenantId) {
        let Some(store) = self.event_store.as_ref() else {
            return;
        };

        match store.list_entity_ids(tenant.as_str()).await {
            Ok(entities) => {
                {
                    let mut index = self.entity_index.write().unwrap(); // ci-ok: infallible lock
                    for (entity_type, entity_id) in &entities {
                        let index_key = format!("{tenant}:{entity_type}");
                        index
                            .entry(index_key)
                            .or_default()
                            .insert(entity_id.clone());
                    }
                } // write lock dropped before metrics call
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

    /// Hydrate actor state from the event store by spawning actors for all
    /// entities that have persisted events in this tenant.
    #[instrument(skip_all, fields(otel.name = "entity.hydrate_from_store", tenant = %tenant))]
    pub async fn hydrate_from_store(&self, tenant: &TenantId) {
        if let Some(ref store) = self.event_store {
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

    /// Get or spawn an entity actor (legacy single-tenant).
    #[deprecated(note = "Use `get_or_spawn_tenant_actor` with explicit tenant")]
    pub fn get_or_spawn_actor(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<ActorRef<EntityMsg>> {
        self.get_or_spawn_tenant_actor(&TenantId::default(), entity_type, entity_id)
    }

    /// Get or spawn an entity actor for a specific tenant.
    pub fn get_or_spawn_tenant_actor(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<ActorRef<EntityMsg>> {
        self.get_or_spawn_tenant_actor_with_fields(
            tenant,
            entity_type,
            entity_id,
            serde_json::json!({}),
        )
    }

    /// Get or spawn an entity actor with initial fields for a specific tenant.
    #[instrument(skip_all, fields(otel.name = "entity.get_or_spawn_tenant_actor_with_fields", tenant = %tenant, entity_type, entity_id))]
    pub fn get_or_spawn_tenant_actor_with_fields(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
    ) -> Option<ActorRef<EntityMsg>> {
        let key = format!("{tenant}:{entity_type}:{entity_id}");

        // Fast-path: check actor registry under read lock.
        {
            let registry = self.actor_registry.read().unwrap();
            if let Some(actor_ref) = registry.get(&key) {
                self.touch_actor_access(&key);
                return Some(actor_ref.clone());
            }
        }

        // Look up live transition table reference: try SpecRegistry first,
        // fall back to legacy map (wrapped in a fresh RwLock for compat).
        let table = {
            let reg = self.registry.read().unwrap();
            reg.get_table_live(tenant, entity_type)
        }
        .or_else(|| {
            // Legacy single-tenant: wrap the static Arc<TransitionTable> in a
            // new RwLock. Hot-swap doesn't apply to legacy mode, but the actor
            // API is uniform. One clone per entity spawn (cheap).
            self.transition_tables
                .get(entity_type)
                .map(|t| Arc::new(RwLock::new((**t).clone())))
        })?;

        // Build actor instance (spawn guarded below to avoid duplicate races).
        let actor = match &self.event_store {
            Some(store) => EntityActor::with_persistence(
                entity_type,
                entity_id,
                table,
                initial_fields,
                store.clone(),
            )
            .with_tenant(tenant.as_str()),
            None => EntityActor::new(entity_type, entity_id, table, initial_fields)
                .with_tenant(tenant.as_str()),
        };

        // Slow-path: atomically re-check and spawn under write lock.
        // This prevents duplicate actors when concurrent requests race to create
        // the same (tenant, entity_type, entity_id) key.
        let actor_ref = {
            let mut registry = self.actor_registry.write().unwrap();
            if let Some(existing) = registry.get(&key) {
                return Some(existing.clone());
            }
            let actor_ref = self.actor_system.spawn(actor, &key);
            registry.insert(key.clone(), actor_ref.clone());
            actor_ref
        };

        // Track in entity index for collection queries
        {
            let index_key = format!("{tenant}:{entity_type}");
            let mut index = self.entity_index.write().unwrap();
            index
                .entry(index_key)
                .or_default()
                .insert(entity_id.to_string());
        }
        self.touch_actor_access(&key);
        runtime_metrics::record_server_state_metrics(self);

        Some(actor_ref)
    }

    /// Remove an entity from the index and actor registry.
    #[instrument(skip_all, fields(otel.name = "entity.remove_entity", tenant = %tenant, entity_type, entity_id))]
    pub fn remove_entity(&self, tenant: &TenantId, entity_type: &str, entity_id: &str) {
        let actor_key = format!("{tenant}:{entity_type}:{entity_id}");

        // Remove from actor registry
        {
            let mut registry = self.actor_registry.write().unwrap();
            registry.remove(&actor_key);
        }
        {
            let mut last_accessed = self.last_accessed.write().unwrap();
            last_accessed.remove(&actor_key);
        }

        // Remove from entity index
        {
            let index_key = format!("{tenant}:{entity_type}");
            let mut index = self.entity_index.write().unwrap();
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
            AuthzDecision::Allow => "Allow",
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
            AuthzDecision::Allow => Ok(()),
            AuthzDecision::Deny(denial) => Err(denial),
        }
    }

    /// Get the current state of an entity actor (legacy single-tenant).
    #[deprecated(note = "Use `get_tenant_entity_state` with explicit tenant")]
    pub async fn get_entity_state(
        &self,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<EntityResponse, String> {
        self.get_tenant_entity_state(&TenantId::default(), entity_type, entity_id)
            .await
    }

    /// Get the current state of an entity actor for a specific tenant.
    #[instrument(skip_all, fields(otel.name = "entity.get_tenant_entity_state", tenant = %tenant, entity_type, entity_id))]
    pub async fn get_tenant_entity_state(
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

        actor_ref
            .ask::<EntityResponse>(EntityMsg::GetState, self.action_dispatch_timeout)
            .await
            .map_err(|e| format!("Actor query failed: {e}"))
    }

    /// Create a new entity with initial fields and return its state.
    #[instrument(skip_all, fields(otel.name = "entity.get_or_create_tenant_entity", tenant = %tenant, entity_type, entity_id))]
    pub async fn get_or_create_tenant_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        let actor_ref = self
            .get_or_spawn_tenant_actor_with_fields(tenant, entity_type, entity_id, initial_fields)
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;

        let response = actor_ref
            .ask::<EntityResponse>(EntityMsg::GetState, self.action_dispatch_timeout)
            .await
            .map_err(|e| format!("Actor query failed: {e}"))?;

        // Broadcast entity creation event for SSE subscribers
        let seq = self.next_entity_event_sequence(tenant.as_str(), entity_type, entity_id);
        let change = EntityStateChange {
            seq,
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
            action: "Created".to_string(),
            status: response.state.status.clone(),
            tenant: tenant.to_string(),
            agent_id: None,
            session_id: None,
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

        Ok(response)
    }

    /// Update fields on an existing entity.
    #[instrument(skip_all, fields(otel.name = "entity.update_tenant_entity_fields", tenant = %tenant, entity_type, entity_id))]
    pub async fn update_tenant_entity_fields(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        replace: bool,
    ) -> Result<EntityResponse, String> {
        let actor_ref = self
            .get_or_spawn_tenant_actor(tenant, entity_type, entity_id)
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;

        actor_ref
            .ask::<EntityResponse>(
                EntityMsg::UpdateFields { fields, replace },
                self.action_dispatch_timeout,
            )
            .await
            .map_err(|e| format!("Actor update failed: {e}"))
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

        let response = actor_ref
            .ask::<EntityResponse>(EntityMsg::Delete, self.action_dispatch_timeout)
            .await
            .map_err(|e| format!("Actor delete failed: {e}"))?;

        if response.success {
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

    /// Ensure an entity is present in memory by lazily hydrating from the
    /// event store when needed.
    #[instrument(skip_all, fields(otel.name = "entity.ensure_entity_loaded", tenant = %tenant, entity_type, entity_id))]
    pub async fn ensure_entity_loaded(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> bool {
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");

        if self.entity_exists(tenant, entity_type, entity_id) {
            let Some(store) = self.event_store.as_ref() else {
                return true;
            };

            let events = match store.read_events(&persistence_id, 0).await {
                Ok(events) if !events.is_empty() => events,
                _ => return true,
            };

            if events.last().is_some_and(is_deleted_envelope) {
                self.remove_entity(tenant, entity_type, entity_id);
                return false;
            }

            return true;
        }

        let Some(store) = self.event_store.as_ref() else {
            return false;
        };

        let events = match store.read_events(&persistence_id, 0).await {
            Ok(events) if !events.is_empty() => events,
            _ => return false,
        };

        if events.last().is_some_and(is_deleted_envelope) {
            self.remove_entity(tenant, entity_type, entity_id);
            return false;
        }

        let Some(actor_ref) = self.get_or_spawn_tenant_actor(tenant, entity_type, entity_id) else {
            return false;
        };

        match actor_ref
            .ask::<EntityResponse>(EntityMsg::GetState, self.action_dispatch_timeout)
            .await
        {
            Ok(response) if response.state.status == "Deleted" => {
                let _ = actor_ref.stop();
                self.remove_entity(tenant, entity_type, entity_id);
                false
            }
            Ok(_) => true,
            Err(_) => {
                self.remove_entity(tenant, entity_type, entity_id);
                false
            }
        }
    }

    /// List entity IDs from in-memory index, lazily hydrating from the event
    /// store if the index is cold.
    #[instrument(skip_all, fields(otel.name = "entity.list_entity_ids_lazy", tenant = %tenant, entity_type))]
    pub async fn list_entity_ids_lazy(&self, tenant: &TenantId, entity_type: &str) -> Vec<String> {
        let ids = self.list_entity_ids(tenant, entity_type);
        if !ids.is_empty() {
            return ids;
        }

        if self.event_store.is_none() {
            return ids;
        }
        self.populate_index_from_store(tenant).await;

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
        for (actor_key, actor_ref) in candidates {
            if let Some(ref store) = self.event_store
                && let Ok(response) = actor_ref
                    .ask::<EntityResponse>(EntityMsg::GetState, self.action_dispatch_timeout)
                    .await
                && response.state.sequence_nr > 0
            {
                // Snapshot excludes bounded in-memory recent event history.
                let mut snapshot_value = match serde_json::to_value(&response.state) {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::warn!(actor_key = %actor_key, error = %e, "failed to encode snapshot value");
                        serde_json::Value::Null
                    }
                };
                if let Some(obj) = snapshot_value.as_object_mut() {
                    obj.remove("events");
                }
                if !snapshot_value.is_null()
                    && let Ok(snapshot_bytes) = serde_json::to_vec(&snapshot_value)
                    && let Err(e) = store
                        .save_snapshot(&actor_key, response.state.sequence_nr, &snapshot_bytes)
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

/// Resolve the current status of an entity.
///
/// Fast path: check the `entity_state_cache` (populated on every successful dispatch).
/// Slow path: fall back to `get_tenant_entity_state()` (async actor ask) and backfill cache.
impl ServerState {
    #[instrument(skip_all, fields(otel.name = "entity.resolve_entity_status", tenant = %tenant, entity_type, entity_id))]
    pub async fn resolve_entity_status(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<String> {
        // Fast path: check cache (LruCache::get requires &mut, so use Mutex).
        let cache_key = format!("{tenant}:{entity_type}:{entity_id}");
        if let Ok(mut cache) = self.entity_state_cache.lock()
            && let Some((status, _timestamp)) = cache.get(&cache_key)
        {
            return Some(status.clone());
        }

        // Slow path: actor ask + backfill
        if let Ok(response) = self
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
        {
            let status = response.state.status.clone();
            self.cache_entity_status(cache_key, status.clone());
            Some(status)
        } else {
            None
        }
    }
}

use temper_authz::{AuthzDecision, AuthzDenial, SecurityContext};
use temper_runtime::actor::ActorRef;
