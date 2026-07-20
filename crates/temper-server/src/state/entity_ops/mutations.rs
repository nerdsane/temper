//! Entity mutation, deletion, and lazy materialization.

use super::*;

impl ServerState {
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
            .get_or_spawn_tenant_actor_when_ready(tenant, entity_type, entity_id)
            .await
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;
        let policy = self.dispatch_retry_policy();
        let fields_for_retry = fields;
        let response = self
            .ask_actor_with_drain_retry::<EntityResponse, _>(
                tenant,
                entity_type,
                entity_id,
                actor_ref,
                || EntityMsg::UpdateFields {
                    fields: fields_for_retry.clone(),
                    replace,
                },
                &policy,
            )
            .await
            .1
            .result
            .map_err(|e| format!("Actor update failed: {e}"))?;

        if response.success
            && let Some(query_plane) = self.query_plane_store()
        {
            let status = response.state.status.clone();
            let fields = self.query_projection_fields(tenant, entity_type, &response.state.fields);
            let projected_state = self.query_projection_state(&response.state);
            let sequence_nr = response.state.sequence_nr;
            let operation = "upsert";
            let source = "field_update";
            record_projection_update_started(tenant, entity_type, operation, source);
            let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
            if let Err(e) = query_plane
                .upsert_projection(
                    tenant.as_str(),
                    entity_type,
                    entity_id,
                    &status,
                    &fields,
                    &projected_state,
                    sequence_nr,
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
                // Same reasoning as create: don't ack a write that won't be
                // visible via $filter. Propagate so OData returns 5xx and
                // clients can retry against the idempotent upsert.
                tracing::error!(
                    error = %e,
                    tenant = %tenant,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "failed to update query projection during field update"
                );
                return Err(format!("query projection write failed during update: {e}"));
            }
            record_projection_update_success(
                tenant,
                entity_type,
                operation,
                source,
                sequence_nr,
                projection_started_at,
            );
        }

        Ok(response)
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
            .get_or_spawn_tenant_actor_when_ready(tenant, entity_type, entity_id)
            .await
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;

        let policy = self.dispatch_retry_policy();
        let (actor_ref, outcome) = self
            .ask_actor_with_drain_retry::<EntityResponse, _>(
                tenant,
                entity_type,
                entity_id,
                actor_ref,
                || EntityMsg::Delete,
                &policy,
            )
            .await;
        let actor_uid = actor_ref.id().uid;
        let response = outcome
            .result
            .map_err(|e| format!("Actor delete failed: {e}"))?;

        if response.success {
            let inactive_timeout_fence = self.reconcile_state_timeout_after_synthetic_commit(
                tenant,
                entity_type,
                entity_id,
                &response.state,
            );
            if let Some(query_plane) = self.query_plane_store() {
                let operation = "remove";
                let source = "delete";
                record_projection_update_started(tenant, entity_type, operation, source);
                let projection_started_at = Instant::now(); // determinism-ok: production-only projection duration metric
                if let Err(e) = query_plane
                    .remove_projection(
                        tenant.as_str(),
                        entity_type,
                        entity_id,
                        response.state.sequence_nr,
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
            if self
                .stop_and_remove_entity_if_current(tenant, entity_type, entity_id, actor_uid)
                .await
            {
                self.release_inactive_state_timeout_after_actor_eviction(
                    tenant,
                    entity_type,
                    entity_id,
                    inactive_timeout_fence,
                );
            }
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

    async fn retire_authoritative_deleted_entity(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        tombstone: &PersistenceEnvelope,
    ) {
        let inactive_timeout_fence = self.fence_state_timeout_after_terminal_event(
            tenant,
            entity_type,
            entity_id,
            tombstone.sequence_nr,
        );
        if self
            .stop_and_remove_entity_incarnation(tenant, entity_type, entity_id, None)
            .await
        {
            self.release_inactive_state_timeout_after_actor_eviction(
                tenant,
                entity_type,
                entity_id,
                inactive_timeout_fence,
            );
        } else {
            tracing::error!(
                tenant = %tenant,
                entity_type,
                entity_id,
                sequence_nr = tombstone.sequence_nr,
                "authoritative tombstone discovered but actor eviction did not complete"
            );
        }
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
        let journal = self.event_journal();

        if self.entity_exists(tenant, entity_type, entity_id) {
            let Some((store, _backend)) = journal.as_ref() else {
                return true;
            };

            let events = match store.read_events(&persistence_id, 0).await {
                Ok(events) if !events.is_empty() => events,
                _ => return true,
            };

            if let Some(tombstone) = events.iter().find(|event| is_deleted_envelope(event)) {
                self.retire_authoritative_deleted_entity(tenant, entity_type, entity_id, tombstone)
                    .await;
                return false;
            }

            return true;
        }

        let Some((store, _backend)) = journal.as_ref() else {
            return false;
        };

        let events = match store.read_events(&persistence_id, 0).await {
            Ok(events) if !events.is_empty() => events,
            _ => return false,
        };

        if let Some(tombstone) = events.iter().find(|event| is_deleted_envelope(event)) {
            self.retire_authoritative_deleted_entity(tenant, entity_type, entity_id, tombstone)
                .await;
            return false;
        }

        self.ensure_entity_actor_materialized(tenant, entity_type, entity_id)
            .await
    }

    /// Spawn an entity actor if needed and wait until its durable state is readable.
    ///
    /// Unlike [`Self::ensure_entity_loaded`], this does not treat an entity-index
    /// entry as proof that an actor is materialized. Out-of-band durable writers
    /// use it after updating the index so they cannot return with a stale or absent
    /// actor for an existing live entity.
    pub(crate) async fn ensure_entity_actor_materialized(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> bool {
        const MATERIALIZATION_ATTEMPT_BUDGET: usize = 3;

        for _ in 0..MATERIALIZATION_ATTEMPT_BUDGET {
            let Some(actor_ref) = self
                .get_or_spawn_tenant_actor_when_ready(tenant, entity_type, entity_id)
                .await
            else {
                return false;
            };
            let policy = self.dispatch_retry_policy();
            let (actor_ref, outcome) = self
                .ask_actor_with_drain_retry::<EntityResponse, _>(
                    tenant,
                    entity_type,
                    entity_id,
                    actor_ref,
                    || EntityMsg::GetState,
                    &policy,
                )
                .await;
            let actor_uid = actor_ref.id().uid;
            match outcome.result {
                Ok(response) if response.state.status == "Deleted" => {
                    self.retire_deleted_hydration_if_current(
                        tenant,
                        entity_type,
                        entity_id,
                        actor_uid,
                        &response,
                    )
                    .await;
                    return false;
                }
                Ok(response) => {
                    let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
                    if self.actor_registry.read().is_ok_and(|registry| {
                        registry.get(&actor_key).is_some_and(|current| {
                            !current.is_stopped() && current.id().uid == actor_uid
                        })
                    }) {
                        self.reconcile_ready_actor_state_timeout(
                            tenant,
                            entity_type,
                            entity_id,
                            actor_uid,
                            &response,
                        );
                        return true;
                    }
                }
                Err(_) => {
                    if self
                        .stop_and_remove_entity_if_current(
                            tenant,
                            entity_type,
                            entity_id,
                            actor_uid,
                        )
                        .await
                    {
                        return false;
                    }
                }
            }
        }

        false
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
}
