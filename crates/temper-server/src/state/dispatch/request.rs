//! Dispatch entry points and request options. The action pipeline is separate
//! so request/cascade routing does not obscure actor admission and persistence.
use super::{DispatchCommand, DispatchError, DispatchExtOptions};
use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use temper_runtime::tenant::TenantId;
use tracing::instrument;

impl crate::state::ServerState {
    /// Dispatch an action using the unified command object.
    ///
    /// This is the preferred entry point. The command struct makes all
    /// parameters explicit (especially tenant) and avoids the previous
    /// three-layer wrapper chain.
    #[instrument(skip_all, fields(
        otel.name = %format_args!("{}.{}", cmd.entity_type, cmd.action),
        tenant = %cmd.tenant,
        entity_type = cmd.entity_type,
        entity_id = cmd.entity_id,
        action_name = cmd.action,
    ))]
    pub async fn dispatch(&self, cmd: DispatchCommand<'_>) -> Result<EntityResponse, String> {
        self.dispatch_typed(cmd).await.map_err(|e| e.to_string())
    }

    /// Dispatch an action to an entity actor (legacy single-tenant).
    #[deprecated(note = "Use `dispatch(DispatchCommand { .. })` with explicit tenant")]
    pub async fn dispatch_action(
        &self,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
    ) -> Result<EntityResponse, String> {
        self.dispatch_tenant_action(
            &TenantId::default(),
            entity_type,
            entity_id,
            action,
            params,
            &AgentContext::for_service("platform-dispatch"),
        )
        .await
    }

    /// Convenience wrapper around [`dispatch`](Self::dispatch) for the common
    /// case where `await_integration` is `false`.
    ///
    /// Callers that need integration await or other options should use
    /// `dispatch(DispatchCommand { .. })` directly.
    #[instrument(skip_all, fields(otel.name = %format_args!("{}.{}", entity_type, action), tenant = %tenant, entity_type, entity_id, action_name = action))]
    pub async fn dispatch_tenant_action(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        agent_ctx: &AgentContext,
    ) -> Result<EntityResponse, String> {
        self.dispatch(DispatchCommand {
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            agent_ctx,
            await_integration: false,
            await_reactions: true,
        })
        .await
    }

    /// Convenience wrapper around [`dispatch`](Self::dispatch) with full options.
    #[instrument(skip_all, fields(otel.name = %format_args!("{}.{}", entity_type, action), tenant = %tenant, entity_type, entity_id, action_name = action))]
    pub async fn dispatch_tenant_action_ext(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        options: DispatchExtOptions<'_>,
    ) -> Result<EntityResponse, String> {
        self.dispatch_tenant_action_ext_typed(
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            options,
        )
        .await
        .map_err(|e| e.to_string())
    }

    /// Typed variant of [`dispatch_tenant_action_ext`](Self::dispatch_tenant_action_ext).
    #[instrument(skip_all, fields(otel.name = %format_args!("{}.{}", entity_type, action), tenant = %tenant, entity_type, entity_id, action_name = action))]
    pub async fn dispatch_tenant_action_ext_typed(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        options: DispatchExtOptions<'_>,
    ) -> Result<EntityResponse, DispatchError> {
        self.dispatch_typed(DispatchCommand {
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            agent_ctx: options.agent_ctx,
            await_integration: options.await_integration,
            await_reactions: options.await_reactions,
        })
        .await
    }

    /// External HTTP admission pins a recovery-only actor. It cannot fall back
    /// to bootstrap if deletion or passivation races with the Cedar check.
    pub(crate) async fn dispatch_bound_action_if_current(
        &self,
        cmd: DispatchCommand<'_>,
        expected_authorization_precondition: String,
    ) -> Result<EntityResponse, DispatchError> {
        if self
            .composite_metadata_for(cmd.tenant, cmd.entity_type, cmd.action)?
            .is_some()
        {
            self.reject_action_supplied_sub_writes(cmd.entity_type, cmd.action, &cmd.params)?;
        }
        let actor = self
            .admit_bound_action(cmd.tenant, cmd.entity_type, cmd.entity_id)
            .await?;
        let (cmd_tenant, cmd_type, cmd_id) = (cmd.tenant, cmd.entity_type, cmd.entity_id);
        let result = self
            .dispatch_action_on_actor(
                cmd,
                Some(expected_authorization_precondition),
                Some(0),
                Some(actor.clone()),
            )
            .await
            .map(|result| result.response);
        if result.is_err() && actor.is_closed() {
            self.remove_failed_bound_actor(&actor);
            // Startup can lose a race to deletion. Classify from authority,
            // never from an opaque stopped/init-failed actor error.
            let snapshot = self
                .load_bound_action_snapshot(cmd_tenant, cmd_type, cmd_id)
                .await
                .map_err(DispatchError::Internal)?;
            if !snapshot.exists {
                return Err(DispatchError::NotFound(format!("{cmd_type}:{cmd_id}")));
            }
        }
        result
    }

    /// Dispatch an externally authorized action only if the target actor still
    /// matches the exact local state used for the Cedar decision.
    #[instrument(skip_all, fields(
        otel.name = %format_args!("{}.{}", cmd.entity_type, cmd.action),
        tenant = %cmd.tenant,
        entity_type = cmd.entity_type,
        entity_id = cmd.entity_id,
        action_name = cmd.action,
    ))]
    pub(crate) async fn dispatch_tenant_action_ext_typed_if_current(
        &self,
        cmd: DispatchCommand<'_>,
        expected_authorization_precondition: String,
    ) -> Result<EntityResponse, DispatchError> {
        self.dispatch_typed_checked(cmd, Some(expected_authorization_precondition))
            .await
    }

    async fn dispatch_typed(
        &self,
        cmd: DispatchCommand<'_>,
    ) -> Result<EntityResponse, DispatchError> {
        self.dispatch_typed_checked(cmd, None).await
    }

    async fn dispatch_typed_checked(
        &self,
        cmd: DispatchCommand<'_>,
        expected_authorization_precondition: Option<String>,
    ) -> Result<EntityResponse, DispatchError> {
        let DispatchCommand {
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            agent_ctx,
            await_integration,
            await_reactions,
        } = cmd;

        if self
            .composite_metadata_for(tenant, entity_type, action)?
            .is_some()
        {
            self.reject_action_supplied_sub_writes(entity_type, action, &params)?;
        }

        self.dispatch_tenant_action_with_completion(
            DispatchCommand {
                tenant,
                entity_type,
                entity_id,
                action,
                params,
                agent_ctx,
                await_integration,
                await_reactions,
            },
            expected_authorization_precondition,
            Some(0),
        )
        .await
        .map(|result| result.response)
    }

    /// Dispatch a deliberate core-only write, without a reaction cascade.
    pub(crate) async fn dispatch_tenant_action_core(
        &self,
        cmd: DispatchCommand<'_>,
        expected_authorization_precondition: Option<String>,
    ) -> Result<EntityResponse, DispatchError> {
        self.dispatch_tenant_action_with_completion(cmd, expected_authorization_precondition, None)
            .await
            .map(|result| result.response)
    }
}
