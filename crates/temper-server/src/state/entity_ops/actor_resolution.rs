//! Generation-aware actor resolution and eviction.

use super::*;

impl ServerState {
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
        self.get_or_spawn_tenant_actor_with_fields_in_context(
            tenant,
            entity_type,
            entity_id,
            serde_json::json!({}),
            None,
        )
    }

    /// Resolve an actor while preserving an already-admitted tenant generation.
    pub(crate) fn get_or_spawn_tenant_actor_in_generation(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        agent_ctx: &AgentContext,
    ) -> Option<ActorRef<EntityMsg>> {
        self.get_or_spawn_tenant_actor_with_fields_in_context(
            tenant,
            entity_type,
            entity_id,
            serde_json::json!({}),
            Some(agent_ctx),
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
        self.get_or_spawn_tenant_actor_with_fields_in_context(
            tenant,
            entity_type,
            entity_id,
            initial_fields,
            None,
        )
    }

    pub(super) fn get_or_spawn_tenant_actor_with_fields_in_context(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
        agent_ctx: Option<&AgentContext>,
    ) -> Option<ActorRef<EntityMsg>> {
        let key = format!("{tenant}:{entity_type}:{entity_id}");
        if self.actor_resolution_gated(tenant, entity_type, agent_ctx) {
            tracing::debug!(
                tenant = %tenant,
                entity_type,
                entity_id,
                "actor spawn deferred while key contract activation is not ready"
            );
            return None;
        }

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
            // Linearize actor insertion against publication arm. If this
            // request observed the old registry before the gate closed, it
            // must not insert that old-table actor after arm's first eviction.
            if self.actor_resolution_gated(tenant, entity_type, agent_ctx) {
                return None;
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

    pub(super) fn actor_resolution_gated(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        agent_ctx: Option<&AgentContext>,
    ) -> bool {
        if self
            .activating_key_contracts
            .read()
            .expect("activating key contracts lock poisoned")
            .contains(&(tenant.as_str().to_string(), entity_type.to_string()))
        {
            return true;
        }
        if !self.spec_publication_gated(tenant) {
            return false;
        }

        !agent_ctx
            .and_then(|ctx| ctx.tenant_generation_lease.as_ref())
            .is_some_and(|lease| {
                lease.belongs_to(tenant)
                    && lease.captured_generation() == self.tenant_generation_version(tenant)
                    && (lease.is_held_for(tenant) || lease.is_publication_owned_for(tenant))
            })
    }

    /// Evict a freshly spawned actor when its first dispatch failed before any
    /// durable history was admitted.
    ///
    /// A failed actor initialization can stop before writing its bootstrap
    /// event. The spawn path has already published the ID to `entity_index` at
    /// that point, so leaving it behind would advertise a phantom entity and
    /// violate index/store agreement. A durable read is the authority: an
    /// ambiguous or non-empty journal is preserved for recovery.
    pub(crate) async fn discard_uncommitted_spawn_after_dispatch_failure(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        expected_actor_id: &temper_runtime::actor::ActorId,
    ) {
        let Some((store, _backend)) = self.event_journal() else {
            self.stop_and_remove_actor_incarnation(
                tenant,
                entity_type,
                entity_id,
                expected_actor_id,
                true,
            );
            return;
        };
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        match store.read_events(&persistence_id, 0).await {
            Ok(events) if events.is_empty() => {
                self.stop_and_remove_actor_incarnation(
                    tenant,
                    entity_type,
                    entity_id,
                    expected_actor_id,
                    true,
                );
            }
            Ok(_) => {
                self.stop_and_remove_actor_incarnation(
                    tenant,
                    entity_type,
                    entity_id,
                    expected_actor_id,
                    false,
                );
            }
            Err(error) => {
                tracing::warn!(
                    tenant = %tenant,
                    entity_type,
                    entity_id,
                    error = %error,
                    "preserving entity index because failed actor durability is ambiguous"
                );
                self.stop_and_remove_actor_incarnation(
                    tenant,
                    entity_type,
                    entity_id,
                    expected_actor_id,
                    false,
                );
            }
        }
    }

    fn stop_and_remove_actor_incarnation(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        expected_actor_id: &temper_runtime::actor::ActorId,
        remove_from_index: bool,
    ) {
        let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
        let Ok(mut registry) = self.actor_registry.write() else {
            return;
        };
        let Some(current) = registry.get(&actor_key) else {
            return;
        };
        if current.id() != expected_actor_id {
            return;
        }
        let removed = registry.remove(&actor_key);
        if let Some(actor_ref) = removed {
            let _ = actor_ref.stop();
        }
        // Keep the registry write lock until the correlated maps are cleared,
        // so a replacement incarnation cannot publish entries between removal
        // and cleanup.
        if let Ok(mut last_accessed) = self.last_accessed.write() {
            last_accessed.remove(&actor_key);
        }
        if remove_from_index && let Ok(mut index) = self.entity_index.write() {
            let index_key = format!("{tenant}:{entity_type}");
            if let Some(ids) = index.get_mut(&index_key) {
                ids.remove(entity_id);
            }
        }
        drop(registry);
        runtime_metrics::record_server_state_metrics(self);
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
}
