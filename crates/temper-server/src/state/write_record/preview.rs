//! Preview the parent and its pending children before an outside write.

use super::*;
use crate::entity_actor::effects::{FieldSyncMode, SpawnRequest};
use tracing::instrument::WithSubscriber;

impl ServerState {
    /// The fields `action` stores when applied to `current`, and the
    /// children its spawns create once it commits, or the preview's refusal.
    pub(super) async fn action_record(
        &self,
        tenant: &TenantId,
        preview: ActionPreview<'_>,
    ) -> Result<(Value, PendingRecords), ActionRecordError> {
        let ActionPreview {
            table,
            current,
            action,
            params,
            related,
            agent_ctx,
        } = preview;
        let (record, result) = self
            .preview_action(tenant, table, current.clone(), action, params, related)
            .await?;
        if !result.success {
            return Err(ActionRecordError::Preview(Box::new(result)));
        }
        let mut pending = PendingRecords::default();
        for spawn in &result.spawn_requests {
            if let Some(child) = self
                .pending_child(tenant, &record, spawn, params, &pending, agent_ctx)
                .await?
            {
                pending.insert(child);
            }
        }
        Ok((record.fields, pending))
    }

    /// `state` after `action`, with the spawns the action requests: the
    /// actor's own action interpreter run on the copy. Unreadable inputs stop
    /// the outside write; an interpreter refusal is kept separately.
    async fn preview_action(
        &self,
        tenant: &TenantId,
        table: &TransitionTable,
        mut state: EntityState,
        action: &str,
        params: &Value,
        related: &RelatedMap,
    ) -> Result<(EntityState, effects::ProcessResult), ConstraintViolation> {
        let blob_store = self.blob_store_for_tenant(tenant).ok();
        let mode = EntityActor::field_sync_mode_for_backend(
            self.event_journal().map(|(_, backend)| backend),
            blob_store.as_ref(),
        );
        // Blob-backed storage preserves domain values: keep the preview's new
        // values inline because their blobs have not been stored yet. Truncating
        // storage changes the value itself, so run exactly that transformation.
        let mode = match mode {
            FieldSyncMode::BlobRefs { .. } => FieldSyncMode::Inline,
            mode => mode,
        };
        // Only the real application of the action should appear in the logs.
        let result = crate::entity_actor::action_input::try_process_action_with_blob_prestate(
            &mut state,
            table,
            action,
            params,
            related,
            mode,
            crate::blobs::BlobReadSource::Tenant {
                state: self,
                tenant,
            },
        )
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await
        .map_err(|error| {
            tracing::warn!(tenant = %tenant, entity_type = %state.entity_type,
                entity_id = %state.entity_id, action, %error, "action preview inputs unreadable");
            ConstraintViolation::stored_value_unreadable(
                &state.entity_type,
                &state.entity_id,
                "action",
            )
        })?;
        Ok((state, result))
    }

    /// The child `spawn` creates once the parent's action (with `params`)
    /// commits, as it will stand after its initial action, from the same plan
    /// the kernel creates it by. `parent` is the parent as the action leaves
    /// it, which is how the child's initial action finds it. `None` when the
    /// kernel creates no child for the spawn.
    async fn pending_child(
        &self,
        tenant: &TenantId,
        parent: &EntityState,
        spawn: &SpawnRequest,
        params: &Value,
        pending: &PendingRecords,
        agent_ctx: &AgentContext,
    ) -> Result<Option<EntityState>, ConstraintViolation> {
        let preceding_child = pending.get(&spawn.entity_type, &spawn.entity_id);
        let Ok(plan) = self
            .spawn_plan(
                tenant,
                &parent.entity_type,
                &parent.entity_id,
                spawn,
                params,
                preceding_child.is_some(),
            )
            .await
        else {
            return Ok(None);
        };
        // Dispatch serializes initializers for the same target in effect order.
        // The next preview therefore starts at the preceding initializer's result.
        let child = if let Some(child) = preceding_child {
            child.clone()
        } else if self
            .ensure_entity_loaded(tenant, &spawn.entity_type, &spawn.entity_id)
            .await
        {
            let Ok(response) = self
                .get_tenant_entity_state(tenant, &spawn.entity_type, &spawn.entity_id)
                .await
            else {
                return Ok(None);
            };
            response.state
        } else {
            EntityActor::build_initial_state(
                &spawn.entity_type,
                &spawn.entity_id,
                &plan.table,
                &plan.initial_fields,
            )
        };
        let Some((action, action_params)) = &plan.initializer else {
            return Ok(Some(child));
        };
        let Ok(action_params) = crate::state::dispatch::bind_authenticated_params(
            &plan.table,
            action,
            action_params.clone(),
            agent_ctx,
        ) else {
            // Live dispatch refuses before applying an initializer whose
            // authenticated inputs cannot be bound, leaving the child as-is.
            return Ok(Some(child));
        };
        let related = self
            .resolve_guards_from_fields(tenant, &spawn.entity_type, action, &child.fields, parent)
            .await;
        let (initialized, result) = self
            .preview_action(
                tenant,
                &plan.table,
                child.clone(),
                action,
                &action_params,
                &related,
            )
            .await?;
        // A refused initial action leaves the child in its preceding state.
        Ok(Some(if result.success { initialized } else { child }))
    }
}
