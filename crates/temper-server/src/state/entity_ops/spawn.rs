//! Spawn, touch, remove, and passivate entity actors.

use super::helpers::actor_idle_timeout_secs;
use crate::entity_actor::{EntityActor, EntityMsg, InProcessEntityRuntime};
use crate::runtime_metrics;
use crate::state::ServerState;
use crate::state::dispatch::retry;
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use temper_runtime::actor::ActorRef;
use temper_runtime::plug::RuntimeRequest;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

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
        // ADR-0048 sub-decision 5: every actor gets the shared idempotency
        // cache so it can dedupe duplicate asks produced by retry storms.
        let tenant_blob_store = self.blob_store_for_tenant(tenant).ok();
        let snapshot_queue = self
            .snapshot_write_queue
            .lock()
            .ok()
            .and_then(|slot| slot.clone());
        let actor = match self.event_journal() {
            Some((store, backend)) => EntityActor::with_persistence(
                entity_type,
                entity_id,
                table,
                initial_fields,
                store,
                backend,
            )
            .with_tenant(tenant.as_str())
            .with_snapshot_queue(snapshot_queue)
            .with_idempotency_cache(self.idempotency_cache.clone())
            .with_blob_store(tenant_blob_store.clone()),
            None => EntityActor::new(entity_type, entity_id, table, initial_fields)
                .with_tenant(tenant.as_str())
                .with_idempotency_cache(self.idempotency_cache.clone())
                .with_blob_store(tenant_blob_store),
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
            // ADR-0048: retry transient failures so passivation is not skipped
            // by a single AskTimeout under load.
            let runtime = InProcessEntityRuntime::new(actor_ref.clone());
            let snapshot_outcome =
                retry::execute_with_backoff(&runtime, RuntimeRequest::GetState, &policy).await;
            if let Some((store, _backend)) = journal.as_ref()
                && let Ok(response) = snapshot_outcome.result
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
}
