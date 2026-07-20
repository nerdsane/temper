use std::sync::{Arc, OnceLock};

use tokio::sync::Semaphore;
use tracing::{Instrument, instrument};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::entity_actor::types::STATE_TIMEOUT_PRECONDITION_MISMATCH;
use crate::entity_actor::{EntityMsg, EntityResponse, StateTimeoutPrecondition};
use crate::request_context::{AgentContext, remote_parent_context};
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use super::effects::PostDispatchContext;
use super::{DispatchCommand, DispatchError, DispatchExtOptions, record_workflow_span_attrs};
use crate::state::admission::AdmissionOutcome;

mod core;

const DEFAULT_BACKGROUND_REACTION_MAX_CONCURRENCY: usize = 64;

fn is_state_timeout_cancellation(is_state_timeout_dispatch: bool, error: Option<&str>) -> bool {
    is_state_timeout_dispatch && error == Some(STATE_TIMEOUT_PRECONDITION_MISMATCH)
}

struct BackgroundReactionDispatch {
    dispatcher: Arc<crate::trigger::ReactionDispatcher>,
    tenant: TenantId,
    entity_type: String,
    entity_id: String,
    action: String,
    to_state: String,
    fields: serde_json::Value,
    agent_ctx: AgentContext,
}

fn background_reaction_semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(
        SEMAPHORE
            .get_or_init(|| Arc::new(Semaphore::new(DEFAULT_BACKGROUND_REACTION_MAX_CONCURRENCY))),
    )
}

impl crate::state::ServerState {
    /// Erase the recursive core-dispatch future used by inline integration
    /// callbacks so nested post-dispatch effects stay heap-backed and bounded.
    #[expect(
        clippy::too_many_arguments,
        reason = "preserves the established core-dispatch call contract while erasing its future"
    )]
    pub(crate) fn dispatch_tenant_action_core<'a>(
        &'a self,
        tenant: &'a TenantId,
        entity_type: &'a str,
        entity_id: &'a str,
        action: &'a str,
        params: serde_json::Value,
        agent_ctx: &'a AgentContext,
        await_integration: bool,
        timeout_precondition: Option<StateTimeoutPrecondition>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<EntityResponse, DispatchError>> + Send + 'a>,
    > {
        Box::pin(self.dispatch_tenant_action_core_inner(
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            agent_ctx,
            await_integration,
            timeout_precondition,
        ))
    }

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

    async fn dispatch_typed(
        &self,
        cmd: DispatchCommand<'_>,
    ) -> Result<EntityResponse, DispatchError> {
        self.dispatch_typed_with_timeout_precondition(cmd, None)
            .await
    }

    /// Dispatch an internally scheduled state timeout with an actor-atomic
    /// state/clock condition. A stale timer returns a benign unsuccessful
    /// response without running post-dispatch effects or reactions.
    pub(crate) async fn dispatch_state_timeout_action(
        &self,
        cmd: DispatchCommand<'_>,
        precondition: StateTimeoutPrecondition,
    ) -> Result<EntityResponse, DispatchError> {
        self.dispatch_typed_with_timeout_precondition(cmd, Some(precondition))
            .await
    }

    async fn dispatch_typed_with_timeout_precondition(
        &self,
        cmd: DispatchCommand<'_>,
        timeout_precondition: Option<StateTimeoutPrecondition>,
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

        let response = self
            .dispatch_tenant_action_core(
                tenant,
                entity_type,
                entity_id,
                action,
                params,
                agent_ctx,
                await_integration,
                timeout_precondition,
            )
            .await?;

        // Dispatch cross-entity reactions (fire-and-forget, depth 0 = top-level)
        if response.success {
            // A poisoned lock must not silently disable reactions: the slot
            // only holds an Arc, so the data can't be torn — recover it.
            let dispatcher = self
                .reaction_dispatcher
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(dispatcher) = dispatcher {
                let to_state = response.state.status.clone();
                let fields = serde_json::to_value(&response.state.fields).unwrap_or_default();
                if await_reactions {
                    dispatcher
                        .dispatch_reactions(
                            self,
                            tenant,
                            entity_type,
                            entity_id,
                            action,
                            &to_state,
                            &fields,
                            0,
                            // ADR-0046: thread the invoking context through so
                            // reactions without an explicit principal inherit
                            // the invoking authority, and declared principals
                            // can elevate deterministically.
                            agent_ctx,
                        )
                        .await;
                } else if let Some(background) =
                    self.try_spawn_background_reactions(BackgroundReactionDispatch {
                        dispatcher: Arc::clone(&dispatcher),
                        tenant: tenant.clone(),
                        entity_type: entity_type.to_string(),
                        entity_id: entity_id.to_string(),
                        action: action.to_string(),
                        to_state,
                        fields,
                        agent_ctx: agent_ctx.clone(),
                    })
                {
                    tracing::warn!(
                        tenant = %tenant,
                        entity_type,
                        entity_id,
                        action_name = action,
                        "background reaction budget exhausted; awaiting reactions inline"
                    );
                    dispatcher
                        .dispatch_reactions(
                            self,
                            tenant,
                            entity_type,
                            entity_id,
                            action,
                            &background.to_state,
                            &background.fields,
                            0,
                            agent_ctx,
                        )
                        .await;
                }
            }
        }

        // Scheduled actions are handled inside run_post_dispatch_effects
        // (called from dispatch_tenant_action_core).
        Ok(response)
    }

    fn try_spawn_background_reactions(
        &self,
        dispatch: BackgroundReactionDispatch,
    ) -> Option<BackgroundReactionDispatch> {
        let Ok(permit) = background_reaction_semaphore().try_acquire_owned() else {
            return Some(dispatch);
        };

        let state = self.clone();
        let BackgroundReactionDispatch {
            dispatcher,
            tenant,
            entity_type,
            entity_id,
            action,
            to_state,
            fields,
            agent_ctx,
        } = dispatch;
        let span = tracing::info_span!(
            "reaction.dispatch.background",
            tenant = %tenant,
            entity_type = %entity_type,
            entity_id = %entity_id,
            action_name = %action,
        );

        let reaction_task = async move {
            let _permit = permit;
            let results = dispatcher
                .dispatch_reactions(
                    &state,
                    &tenant,
                    &entity_type,
                    &entity_id,
                    &action,
                    &to_state,
                    &fields,
                    0,
                    &agent_ctx,
                )
                .await;
            tracing::info!(
                tenant = %tenant,
                entity_type = %entity_type,
                entity_id = %entity_id,
                action_name = %action,
                reaction.result_count = results.len(),
                "background reactions completed"
            );
        }
        .instrument(span);
        tokio::spawn(reaction_task); // determinism-ok: production-only post-commit reaction side effects
        None
    }
}

#[cfg(test)]
mod timeout_cancellation_tests {
    use super::*;

    #[test]
    fn matching_domain_error_is_not_a_normal_dispatch_cancellation() {
        assert!(!is_state_timeout_cancellation(
            false,
            Some(STATE_TIMEOUT_PRECONDITION_MISMATCH),
        ));
        assert!(is_state_timeout_cancellation(
            true,
            Some(STATE_TIMEOUT_PRECONDITION_MISMATCH),
        ));
    }
}
