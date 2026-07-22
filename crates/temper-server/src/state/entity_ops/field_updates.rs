//! Generation-aware entity field updates.

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
        idempotency_key: Option<String>,
    ) -> Result<EntityResponse, String> {
        self.update_tenant_entity_fields_with_context(
            tenant,
            entity_type,
            entity_id,
            fields,
            replace,
            idempotency_key,
            None,
        )
        .await
    }

    /// Update entity fields inside an already-admitted tenant generation.
    #[expect(
        clippy::too_many_arguments,
        reason = "generation-aware update preserves the public field-update contract"
    )]
    pub async fn update_tenant_entity_fields_in_generation(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        replace: bool,
        idempotency_key: Option<String>,
        agent_ctx: &AgentContext,
    ) -> Result<EntityResponse, String> {
        self.update_tenant_entity_fields_with_context(
            tenant,
            entity_type,
            entity_id,
            fields,
            replace,
            idempotency_key,
            Some(agent_ctx),
        )
        .await
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "internal field update carries the optional captured agent context"
    )]
    pub(super) async fn update_tenant_entity_fields_with_context(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        fields: serde_json::Value,
        replace: bool,
        idempotency_key: Option<String>,
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

        let policy = self.dispatch_retry_policy();
        let fields_for_retry = fields;
        let idempotency_key =
            idempotency_key.unwrap_or_else(|| format!("field-update:{}", sim_uuid()));
        let response = retry::ask_with_backoff::<_, EntityResponse, _>(
            &actor_ref,
            || EntityMsg::UpdateFields {
                fields: fields_for_retry.clone(),
                replace,
                idempotency_key: idempotency_key.clone(),
            },
            &policy,
        )
        .await
        .result
        .map_err(|e| format!("Actor update failed: {e}"))?;

        if !response.success {
            return Err(response
                .error
                .clone()
                .unwrap_or_else(|| "field update was rejected".to_string()));
        }

        if let Some(query_plane) = self.query_plane_store() {
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
}
