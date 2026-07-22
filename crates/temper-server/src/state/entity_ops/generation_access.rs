//! Generation-aware entity reads and creates.

use super::*;

impl ServerState {
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
        self.get_tenant_entity_state_with_context(tenant, entity_type, entity_id, None)
            .await
    }

    /// Get entity state inside a tenant generation already admitted by dispatch.
    pub async fn get_tenant_entity_state_in_generation(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        agent_ctx: &AgentContext,
    ) -> Result<EntityResponse, String> {
        self.get_tenant_entity_state_with_context(tenant, entity_type, entity_id, Some(agent_ctx))
            .await
    }

    pub(super) async fn get_tenant_entity_state_with_context(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        agent_ctx: Option<&AgentContext>,
    ) -> Result<EntityResponse, String> {
        let actor_ref = self
            .get_or_spawn_tenant_actor_with_fields_in_context(
                tenant,
                entity_type,
                entity_id,
                serde_json::json!({}),
                agent_ctx,
            )
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;

        // ADR-0048: retry transient ask failures (AskTimeout, MailboxFull)
        // so a single slow actor reply does not surface as HTTP 500.
        let policy = self.dispatch_retry_policy();
        retry::ask_with_backoff::<_, EntityResponse, _>(&actor_ref, || EntityMsg::GetState, &policy)
            .await
            .result
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
        self.get_or_create_tenant_entity_with_context(
            tenant,
            entity_type,
            entity_id,
            initial_fields,
            None,
        )
        .await
    }

    /// Create or resolve an entity inside an already-admitted tenant generation.
    pub async fn get_or_create_tenant_entity_in_generation(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
        agent_ctx: &AgentContext,
    ) -> Result<EntityResponse, String> {
        self.get_or_create_tenant_entity_with_context(
            tenant,
            entity_type,
            entity_id,
            initial_fields,
            Some(agent_ctx),
        )
        .await
    }

    pub(super) async fn get_or_create_tenant_entity_with_context(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        initial_fields: serde_json::Value,
        agent_ctx: Option<&AgentContext>,
    ) -> Result<EntityResponse, String> {
        let actor_ref = self
            .get_or_spawn_tenant_actor_with_fields_in_context(
                tenant,
                entity_type,
                entity_id,
                initial_fields,
                agent_ctx,
            )
            .ok_or_else(|| {
                format!("No transition table for tenant '{tenant}', entity type '{entity_type}'")
            })?;

        let policy = self.dispatch_retry_policy();
        let response = retry::ask_with_backoff::<_, EntityResponse, _>(
            &actor_ref,
            || EntityMsg::GetState,
            &policy,
        )
        .await
        .result
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

        if let Some(query_plane) = self.query_plane_store() {
            let status = response.state.status.clone();
            let fields = self.query_projection_fields(tenant, entity_type, &response.state.fields);
            let projected_state = self.query_projection_state(&response.state);
            let sequence_nr = response.state.sequence_nr;
            let operation = "upsert";
            let source = "create";
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
                // Surface the projection failure instead of swallowing it.
                // Previously this was a `warn` followed by `Ok(response)`,
                // which produced HTTP 201 even when the entity wasn't
                // discoverable via $filter — silently corrupting any caller
                // that does write-then-read-back. Returning Err propagates
                // up through the OData write handler to a 5xx so the client
                // knows to retry. The event is already durable in the
                // events table; on retry the actor's idempotent UPSERT into
                // entity_catalog/entity_field_index will succeed (or fail
                // again loudly).
                tracing::error!(
                    error = %e,
                    tenant = %tenant,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    "failed to update query projection during create"
                );
                return Err(format!("query projection write failed during create: {e}"));
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
}
