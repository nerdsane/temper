//! Generation-aware lazy entity hydration.

use super::*;

impl ServerState {
    /// Ensure an entity is present in memory by lazily hydrating from the
    /// event store when needed.
    #[instrument(skip_all, fields(otel.name = "entity.ensure_entity_loaded", tenant = %tenant, entity_type, entity_id))]
    pub async fn ensure_entity_loaded(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> bool {
        self.ensure_entity_loaded_with_context(tenant, entity_type, entity_id, None)
            .await
    }

    /// Hydrate an entity inside an already-admitted tenant generation.
    pub async fn ensure_entity_loaded_in_generation(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        agent_ctx: &AgentContext,
    ) -> bool {
        self.ensure_entity_loaded_with_context(tenant, entity_type, entity_id, Some(agent_ctx))
            .await
    }

    pub(super) async fn ensure_entity_loaded_with_context(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        agent_ctx: Option<&AgentContext>,
    ) -> bool {
        let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
        let Some((store, _backend)) = self.event_journal() else {
            return self.entity_exists(tenant, entity_type, entity_id);
        };

        // Snapshot replacement and the first materializing journal append are
        // stream-fenced, but can race this read. Retry a bounded number of exact
        // source captures instead of accepting an actor hydrated from a source
        // generation that changed underneath the existence check.
        for _attempt in 0..3 {
            let resident = self.entity_exists(tenant, entity_type, entity_id);
            let boundary = match store.journal_boundary(&persistence_id).await {
                Ok(boundary) => boundary,
                Err(_) => return resident,
            };
            if boundary.first_terminal_sequence.is_some() {
                self.remove_entity(tenant, entity_type, entity_id);
                return false;
            }
            if resident && boundary.latest_sequence > 0 {
                return true;
            }

            let captured_snapshot = if boundary.latest_sequence == 0 {
                match store.load_snapshot(&persistence_id).await {
                    Ok(Some(snapshot)) => Some(snapshot),
                    Ok(None) => return resident,
                    Err(_) => return resident,
                }
            } else {
                None
            };

            let Some(actor_ref) = self.get_or_spawn_tenant_actor_with_fields_in_context(
                tenant,
                entity_type,
                entity_id,
                serde_json::json!({}),
                agent_ctx,
            ) else {
                return false;
            };
            let policy = self.dispatch_retry_policy();
            if let Some(captured_snapshot) = captured_snapshot {
                let outcome = retry::ask_with_backoff::<_, EntityPassivationSnapshot, _>(
                    &actor_ref,
                    || EntityMsg::GetPassivationSnapshot,
                    &policy,
                )
                .await;
                let response = match outcome.result {
                    Ok(response) => response,
                    Err(_) => {
                        self.stop_and_remove_entity(tenant, entity_type, entity_id);
                        return false;
                    }
                };
                if response.state.status == "Deleted" {
                    self.stop_and_remove_entity(tenant, entity_type, entity_id);
                    return false;
                }
                let actor_used_captured_source = response.snapshot_source
                    == (SnapshotSourceFence::Exact {
                        sequence_nr: captured_snapshot.0,
                        state: captured_snapshot.1.clone(),
                    });
                let source_still_exact = matches!(
                    (
                        store.journal_boundary(&persistence_id).await,
                        store.load_snapshot(&persistence_id).await,
                    ),
                    (Ok(boundary), Ok(Some(current_snapshot)))
                        if boundary.latest_sequence == 0 && current_snapshot == captured_snapshot
                );
                if !actor_used_captured_source || !source_still_exact {
                    self.stop_and_remove_entity(tenant, entity_type, entity_id);
                    continue;
                }
            } else {
                let outcome = retry::ask_with_backoff::<_, EntityResponse, _>(
                    &actor_ref,
                    || EntityMsg::GetState,
                    &policy,
                )
                .await;
                match outcome.result {
                    Ok(response) if response.state.status == "Deleted" => {
                        self.stop_and_remove_entity(tenant, entity_type, entity_id);
                        return false;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        self.stop_and_remove_entity(tenant, entity_type, entity_id);
                        return false;
                    }
                }
            }
            return true;
        }

        false
    }
}
