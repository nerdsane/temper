//! Projection verification, actor lookup, and authorization.

use super::*;

impl ServerState {
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
        self.ensure_registry_timeout_reconciliation_started();
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
        self.ensure_registry_timeout_reconciliation_started();
        let key = format!("{tenant}:{entity_type}:{entity_id}");

        // Fast-path: check actor registry under read lock.
        {
            let registry = self.actor_registry.read().unwrap();
            if let Some(actor_ref) = registry.get(&key) {
                if actor_ref.is_draining() {
                    return None;
                }
                if !actor_ref.is_stopped() {
                    self.touch_actor_access(&key);
                    return Some(actor_ref.clone());
                }
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
        let (actor_ref, timeout_hydration, hydration_completion) = {
            let mut registry = self.actor_registry.write().unwrap();
            if let Some(existing) = registry.get(&key) {
                if existing.is_draining() {
                    return None;
                }
                if !existing.is_stopped() {
                    return Some(existing.clone());
                }
            }
            if let Some(stopped) = registry.get(&key) {
                tracing::warn!(
                    actor_key = %key,
                    actor_uid = %stopped.id().uid,
                    "replacing stopped actor registry entry"
                );
            }
            let (actor_ref, timeout_hydration) = match self
                .actor_system
                .spawn_with_first_ask::<_, EntityResponse>(actor, &key, EntityMsg::GetState)
            {
                Ok(spawned) => spawned,
                Err(error) => {
                    tracing::error!(
                        tenant = %tenant,
                        entity_type,
                        entity_id,
                        error = %error,
                        "actor spawn failed to admit startup reconciliation"
                    );
                    return None;
                }
            };
            let hydration_completion = self.state_timeout_tracker.register_hydration(
                tenant,
                entity_type,
                entity_id,
                actor_ref.id().uid,
            );
            registry.insert(key.clone(), actor_ref.clone());
            (actor_ref, timeout_hydration, hydration_completion)
        };

        // ADR-0191: actor spawn is the common lifecycle boundary for eager
        // restart hydration, lazy durable loads, and passivation respawn.
        // Reconcile the declared timeout without waiting for request traffic.
        self.schedule_state_timeout_hydration(
            tenant,
            entity_type,
            entity_id,
            actor_ref.id().uid,
            timeout_hydration,
            hydration_completion,
        );

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
}
