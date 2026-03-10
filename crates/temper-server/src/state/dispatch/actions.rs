use tracing::instrument;

use crate::entity_actor::{EntityMsg, EntityResponse, EntityState};
use crate::request_context::AgentContext;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use super::effects::PostDispatchContext;
use super::{DispatchCommand, DispatchError, DispatchExtOptions};

impl crate::state::ServerState {
    /// Dispatch an action using the unified command object.
    ///
    /// This is the preferred entry point. The command struct makes all
    /// parameters explicit (especially tenant) and avoids the previous
    /// three-layer wrapper chain.
    #[instrument(skip_all, fields(
        otel.name = "dispatch.dispatch",
        tenant = %cmd.tenant,
        entity_type = cmd.entity_type,
        entity_id = cmd.entity_id,
        action_name = cmd.action,
    ))]
    pub async fn dispatch(&self, cmd: DispatchCommand<'_>) -> Result<EntityResponse, String> {
        let response = self
            .dispatch_tenant_action_core(
                cmd.tenant,
                cmd.entity_type,
                cmd.entity_id,
                cmd.action,
                cmd.params,
                cmd.agent_ctx,
                cmd.await_integration,
            )
            .await
            .map_err(|e| e.to_string())?;

        // Dispatch cross-entity reactions (fire-and-forget, depth 0 = top-level)
        if response.success {
            let dispatcher = self
                .reaction_dispatcher
                .read()
                .ok()
                .and_then(|slot| slot.clone());
            if let Some(dispatcher) = dispatcher {
                let fields = serde_json::to_value(&response.state.fields).unwrap_or_default();
                dispatcher
                    .dispatch_reactions(
                        self,
                        cmd.tenant,
                        cmd.entity_type,
                        cmd.entity_id,
                        cmd.action,
                        &response.state.status,
                        &fields,
                        0,
                    )
                    .await;
            }
        }

        // Scheduled actions are handled inside run_post_dispatch_effects
        // (called from dispatch_tenant_action_core).

        Ok(response)
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
            &AgentContext::system(),
        )
        .await
    }

    /// Convenience wrapper around [`dispatch`](Self::dispatch) for the common
    /// case where `await_integration` is `false`.
    ///
    /// Callers that need integration await or other options should use
    /// `dispatch(DispatchCommand { .. })` directly.
    #[instrument(skip_all, fields(otel.name = "dispatch.dispatch_tenant_action", tenant = %tenant, entity_type, entity_id, action_name = action))]
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
        })
        .await
    }

    /// Convenience wrapper around [`dispatch`](Self::dispatch) with full options.
    #[instrument(skip_all, fields(otel.name = "dispatch.dispatch_tenant_action_ext", tenant = %tenant, entity_type, entity_id, action_name = action))]
    pub async fn dispatch_tenant_action_ext(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        options: DispatchExtOptions<'_>,
    ) -> Result<EntityResponse, String> {
        self.dispatch(DispatchCommand {
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            agent_ctx: options.agent_ctx,
            await_integration: options.await_integration,
        })
        .await
    }

    /// Core dispatch without reaction cascade (used by ReactionDispatcher to
    /// avoid infinite async recursion).
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(otel.name = "dispatch.dispatch_tenant_action_core", tenant = %tenant, entity_type, entity_id, action_name = action))]
    pub(crate) async fn dispatch_tenant_action_core(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        agent_ctx: &AgentContext,
        await_integration: bool,
    ) -> Result<EntityResponse, DispatchError> {
        let Some(actor_ref) = self.get_or_spawn_tenant_actor(tenant, entity_type, entity_id) else {
            // Spec-free dispatch: no transition table, but Cedar allowed the action.
            let entry = TrajectoryEntry {
                timestamp: sim_now().to_rfc3339(),
                tenant: tenant.to_string(),
                entity_type: entity_type.to_string(),
                entity_id: entity_id.to_string(),
                action: action.to_string(),
                success: true,
                from_status: None,
                to_status: None,
                error: None,
                agent_id: agent_ctx.agent_id.clone(),
                session_id: agent_ctx.session_id.clone(),
                authz_denied: None,
                denied_resource: None,
                denied_module: None,
                source: Some(TrajectorySource::Entity),
                spec_governed: Some(false),
                agent_type: agent_ctx.agent_type.clone(),
            };
            if let Err(e) = self.persist_trajectory_entry(&entry).await {
                tracing::error!(error = %e, "failed to persist trajectory entry");
            }
            return Ok(EntityResponse {
                success: true,
                state: EntityState {
                    entity_type: entity_type.to_string(),
                    entity_id: entity_id.to_string(),
                    status: String::new(),
                    item_count: 0,
                    counters: std::collections::BTreeMap::new(),
                    booleans: std::collections::BTreeMap::new(),
                    lists: std::collections::BTreeMap::new(),
                    fields: serde_json::json!({}),
                    events: vec![],
                    sequence_nr: 0,
                },
                error: None,
                custom_effects: vec![],
                scheduled_actions: vec![],
                spawn_requests: vec![],
                spec_governed: false,
            });
        };

        // Pre-resolve cross-entity state gates (Gap 1: Agent OS).
        let cross_entity_booleans = self
            .resolve_cross_entity_guards(tenant, entity_type, entity_id, action)
            .await;

        let action_params = params.clone();
        let response = match actor_ref
            .ask::<EntityResponse>(
                EntityMsg::Action {
                    name: action.to_string(),
                    params,
                    cross_entity_booleans,
                },
                self.action_dispatch_timeout,
            )
            .await
        {
            Ok(response) => response,
            Err(e) => {
                let entry = TrajectoryEntry {
                    timestamp: sim_now().to_rfc3339(),
                    tenant: tenant.to_string(),
                    entity_type: entity_type.to_string(),
                    entity_id: entity_id.to_string(),
                    action: action.to_string(),
                    success: false,
                    from_status: None,
                    to_status: None,
                    error: Some(format!("Actor dispatch failed: {e}")),
                    agent_id: agent_ctx.agent_id.clone(),
                    session_id: agent_ctx.session_id.clone(),
                    authz_denied: None,
                    denied_resource: None,
                    denied_module: None,
                    source: Some(TrajectorySource::Entity),
                    spec_governed: None,
                    agent_type: agent_ctx.agent_type.clone(),
                };
                if let Err(persist_err) = self.persist_trajectory_entry(&entry).await {
                    tracing::error!(error = %persist_err, "failed to persist trajectory entry");
                }
                return Err(DispatchError::ActorFailed(e.to_string()));
            }
        };

        // Run all post-dispatch effects through the dedicated pipeline.
        let ctx = PostDispatchContext {
            tenant,
            entity_type,
            entity_id,
            action,
            agent_ctx,
            action_params: &action_params,
            await_integration,
        };
        let response = self.run_post_dispatch_effects(&ctx, response).await;

        Ok(response)
    }
}
