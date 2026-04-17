use tracing::instrument;

use crate::entity_actor::{EntityMsg, EntityResponse};
use crate::request_context::AgentContext;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use super::effects::PostDispatchContext;
use super::retry;
use super::{DispatchCommand, DispatchError, DispatchExtOptions};
use crate::state::admission::AdmissionOutcome;

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
            &AgentContext::system(),
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
        })
        .await
    }

    async fn dispatch_typed(
        &self,
        cmd: DispatchCommand<'_>,
    ) -> Result<EntityResponse, DispatchError> {
        let DispatchCommand {
            tenant,
            entity_type,
            entity_id,
            action,
            params,
            agent_ctx,
            await_integration,
        } = cmd;

        let response = self
            .dispatch_tenant_action_core(
                tenant,
                entity_type,
                entity_id,
                action,
                params,
                agent_ctx,
                await_integration,
            )
            .await?;

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
                        tenant,
                        entity_type,
                        entity_id,
                        action,
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

    /// Core dispatch without reaction cascade (used by ReactionDispatcher to
    /// avoid infinite async recursion).
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(
        otel.name = "dispatch.dispatch_tenant_action_core",
        tenant = %tenant,
        entity_type,
        entity_id,
        action_name = action,
        success = tracing::field::Empty,
        error_msg = tracing::field::Empty,
    ))]
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
        if !self
            .is_entity_type_governed(tenant, entity_type)
            .map_err(DispatchError::Internal)?
        {
            // Default-deny: entity type has no registered spec.
            tracing::warn!(
                tenant = %tenant,
                entity_type,
                entity_id,
                action,
                "rejecting action on ungoverned entity type (no spec registered)"
            );
            tracing::warn!(
                tenant = %tenant,
                entity_type,
                entity_id,
                action,
                source = "Entity",
                authz_denied = false,
                "unmet_intent"
            );
            return Err(DispatchError::Ungoverned(entity_type.to_string()));
        }

        let Some(actor_ref) = self.get_or_spawn_tenant_actor(tenant, entity_type, entity_id) else {
            return Err(DispatchError::Internal(format!(
                "failed to resolve actor for governed entity type '{entity_type}'"
            )));
        };

        // Pre-resolve cross-entity state gates (Gap 1: Agent OS).
        let cross_entity_booleans = self
            .resolve_cross_entity_guards(tenant, entity_type, entity_id, action)
            .await;

        let action_params = params.clone();
        let admission_start = std::time::Instant::now();
        // ADR-0051: acquire an admission permit before spending retry budget.
        // Caps are pulled inline from the spec registry so `[admission]`
        // declarations take effect at spec load with no separate
        // registration step.
        let admission_caps = self.admission_caps_for(tenant, entity_type);
        let admission_was_capped = admission_caps.is_some();
        let _admission_permit = match self
            .admission
            .try_acquire_with_caps(tenant, entity_type, action, admission_caps.as_ref())
            .await
        {
            AdmissionOutcome::Passthrough => None,
            AdmissionOutcome::Granted(permit) => {
                let waited = admission_start.elapsed();
                crate::runtime_metrics::record_admission_granted(
                    tenant.as_str(),
                    entity_type,
                    action,
                    waited,
                );
                if waited > std::time::Duration::from_millis(1) {
                    crate::runtime_metrics::record_admission_queued(
                        tenant.as_str(),
                        entity_type,
                        action,
                    );
                }
                Some(permit)
            }
            AdmissionOutcome::Deferred {
                retry_after_ms,
                waited,
            } => {
                crate::runtime_metrics::record_admission_deferred(
                    tenant.as_str(),
                    entity_type,
                    action,
                    waited,
                );
                crate::runtime_metrics::record_dispatch_outcome(
                    tenant.as_str(),
                    entity_type,
                    action,
                    crate::runtime_metrics::DispatchOutcome::Deferred,
                    0,
                    admission_start.elapsed(),
                );
                return Err(DispatchError::Deferred { retry_after_ms });
            }
        };
        let _ = admission_was_capped; // reserved for active-permits gauge
        // ADR-0048: wrap the ask in a bounded retry that classifies
        // transient (AskTimeout / MailboxFull) vs permanent failures.
        let policy = self.dispatch_retry_policy();
        let action_name = action.to_string();
        let params_for_retry = params;
        let cross_for_retry = cross_entity_booleans;
        let idempotency_key = agent_ctx.idempotency_key.clone();
        let outcome = retry::ask_with_backoff::<_, EntityResponse, _>(
            &actor_ref,
            || EntityMsg::Action {
                name: action_name.clone(),
                params: params_for_retry.clone(),
                cross_entity_booleans: cross_for_retry.clone(),
                idempotency_key: idempotency_key.clone(),
            },
            &policy,
        )
        .await;
        // ADR-0048: emit dispatch outcome / attempts / latency metrics.
        let ask_outcome_for_metrics = match &outcome.result {
            Ok(_) => {
                if outcome.retried_after_transient {
                    crate::runtime_metrics::DispatchOutcome::TransientRetriedOk
                } else {
                    crate::runtime_metrics::DispatchOutcome::Ok
                }
            }
            Err(e) => {
                let kind: &'static str = match e {
                    temper_runtime::actor::ActorError::AskTimeout(_) => "ask_timeout",
                    temper_runtime::actor::ActorError::MailboxFull => "mailbox_full",
                    temper_runtime::actor::ActorError::Stopped => "stopped",
                    temper_runtime::actor::ActorError::SendFailed => "send_failed",
                    temper_runtime::actor::ActorError::Panicked(_) => "panicked",
                    temper_runtime::actor::ActorError::InitFailed(_) => "init_failed",
                    temper_runtime::actor::ActorError::MaxRestartsExceeded(_) => {
                        "max_restarts_exceeded"
                    }
                    temper_runtime::actor::ActorError::Custom(_) => "custom",
                };
                crate::runtime_metrics::record_dispatch_error(
                    tenant.as_str(),
                    entity_type,
                    action,
                    kind,
                );
                if e == &temper_runtime::actor::ActorError::MailboxFull {
                    crate::runtime_metrics::record_actor_mailbox_full_drop(entity_type, action);
                }
                if e.is_transient() {
                    crate::runtime_metrics::DispatchOutcome::TransientExhausted
                } else {
                    crate::runtime_metrics::DispatchOutcome::Permanent
                }
            }
        };
        crate::runtime_metrics::record_dispatch_outcome(
            tenant.as_str(),
            entity_type,
            action,
            ask_outcome_for_metrics,
            outcome.attempts,
            outcome.elapsed,
        );

        let response = match outcome.result {
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
                    request_body: Some(action_params.clone()),
                    intent: agent_ctx.intent.clone(),
                    matched_policy_ids: None,
                };
                let request_body_str = {
                    let s = action_params.to_string();
                    if s.len() > 4096 {
                        format!("{}[truncated]", &s[..4096])
                    } else {
                        s
                    }
                };
                tracing::info!(
                    tenant = %entry.tenant,
                    entity_type = %entry.entity_type,
                    entity_id = %entry.entity_id,
                    action = %entry.action,
                    success = entry.success,
                    from_status = ?entry.from_status,
                    to_status = ?entry.to_status,
                    error = ?entry.error,
                    source = ?entry.source,
                    authz_denied = ?entry.authz_denied,
                    spec_governed = ?entry.spec_governed,
                    request_body = %request_body_str,
                    "trajectory.entry"
                );
                if !entry.success {
                    tracing::warn!(
                        tenant = %entry.tenant,
                        entity_type = %entry.entity_type,
                        entity_id = %entry.entity_id,
                        action = %entry.action,
                        error = ?entry.error,
                        authz_denied = ?entry.authz_denied,
                        source = ?entry.source,
                        "unmet_intent"
                    );
                }
                tracing::Span::current().record("success", false);
                if let Some(ref err) = entry.error {
                    tracing::Span::current().record("error_msg", err.as_str());
                }
                if let Err(persist_err) = self.persist_trajectory_entry(&entry).await {
                    tracing::error!(error = %persist_err, "failed to persist trajectory entry");
                }
                return Err(DispatchError::from_actor_error(e, outcome.attempts));
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

        tracing::Span::current().record("success", response.success);
        if let Some(ref err) = response.error {
            tracing::Span::current().record("error_msg", err.as_str());
        }

        Ok(response)
    }
}
