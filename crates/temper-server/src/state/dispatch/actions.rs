use std::sync::{Arc, OnceLock};

use tokio::sync::Semaphore;
use tracing::{Instrument, instrument};

use crate::entity_actor::{EntityMsg, EntityResponse};
use crate::request_context::AgentContext;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use super::effects::PostDispatchContext;
use super::retry;
use super::{DispatchCommand, DispatchError, DispatchExtOptions, record_workflow_span_attrs};
use crate::state::admission::AdmissionOutcome;
use crate::state::write_record::WriteCaller;

const DEFAULT_BACKGROUND_REACTION_MAX_CONCURRENCY: usize = 64;

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

/// Maximum request-body length, in bytes, echoed into the dispatch-failure log.
const LOG_REQUEST_BODY_MAX_BYTES: usize = 4096;

/// Bound a serialized request body for the failure log line.
///
/// Slicing at a fixed byte offset panics when the cap lands inside a multi-byte
/// character, which turns a logged dispatch failure into a second failure. The
/// cut walks back to the nearest character boundary instead.
fn truncate_request_body_for_log(serialized: &str) -> String {
    if serialized.len() <= LOG_REQUEST_BODY_MAX_BYTES {
        return serialized.to_string();
    }
    let mut end = LOG_REQUEST_BODY_MAX_BYTES;
    while end > 0 && !serialized.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}[truncated]", &serialized[..end])
}

/// `agent_ctx` with a key for the move it sends: the caller's, or one made up
/// for an unkeyed caller. Everything the move's commit sets off (its answer,
/// reactions and children) derives its own key from this one, so a write the
/// move owes carries the same key on every try (see `owed_writes`).
fn keyed_for_move(
    mut agent_ctx: AgentContext,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    action: &str,
) -> AgentContext {
    if agent_ctx.idempotency_key.is_none() {
        agent_ctx.idempotency_key = Some(crate::idempotency::IdempotencyKey::Request(format!(
            "dispatch:{tenant}:{entity_type}:{entity_id}:{action}:{}",
            sim_uuid()
        )));
    }
    agent_ctx
}

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
        caller: WriteCaller,
        expected_authorization_precondition: String,
    ) -> Result<EntityResponse, DispatchError> {
        self.dispatch_typed_checked(cmd, caller, Some(expected_authorization_precondition))
            .await
    }

    /// Dispatch an action a caller outside the kernel sent, such as an
    /// HttpEndpoint route or an inbound webhook, with no Cedar precondition
    /// to hold the actor to.
    pub(crate) async fn dispatch_outside_action(
        &self,
        cmd: DispatchCommand<'_>,
    ) -> Result<EntityResponse, DispatchError> {
        self.dispatch_typed_checked(cmd, WriteCaller::Outside, None)
            .await
    }

    pub(super) async fn dispatch_typed(
        &self,
        cmd: DispatchCommand<'_>,
    ) -> Result<EntityResponse, DispatchError> {
        self.dispatch_typed_checked(cmd, WriteCaller::Kernel, None)
            .await
    }

    /// The dispatch, on the task [`Self::dispatch_typed_checked`] gives it.
    pub(super) async fn dispatch_typed_attached(
        &self,
        cmd: DispatchCommand<'_>,
        caller: WriteCaller,
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
        // This action is a step of the inline chain its integrations and
        // reactions continue (`InlineChain`), under the key it commits with.
        // Keyed here rather than in the core dispatch, so the reactions below
        // derive their keys from that key.
        let entered_ctx = keyed_for_move(
            agent_ctx.entering(entity_type, entity_id, action),
            tenant,
            entity_type,
            entity_id,
            action,
        );
        let agent_ctx = &entered_ctx;

        if temper_spec::automaton::is_carry_move(action) {
            return Err(DispatchError::AuthzDenied(format!(
                "'{action}' is a [[carry]] move, which only the platform makes when it installs \
                 a new version of the spec"
            )));
        }

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
                caller,
                expected_authorization_precondition,
            )
            .await?;

        // Dispatch cross-entity reactions (fire-and-forget, depth 0 = top-level)
        if response.success {
            // The children this action spawned are entities its chain made.
            let reaction_ctx = entered_ctx.having_made(
                response
                    .spawn_requests
                    .iter()
                    .map(|spawn| (spawn.entity_type.as_str(), spawn.entity_id.as_str())),
            );
            let agent_ctx = &reaction_ctx;
            // A poisoned lock must not silently disable reactions: the slot
            // only holds an Arc, so the data can't be torn — recover it.
            let dispatcher = self
                .reaction_dispatcher
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            if let Some(dispatcher) = dispatcher {
                let action = response.action(action);
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
        let span = crate::trace_context::runtime_work_span!(
            crate::trace_context::cause(),
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

    /// Core dispatch without reaction cascade (used by ReactionDispatcher to
    /// avoid infinite async recursion).
    #[allow(clippy::too_many_arguments)]
    #[instrument(skip_all, fields(
        otel.name = "dispatch.dispatch_tenant_action_core",
        tenant = %tenant,
        entity_type,
        entity_id,
        action_name = action,
        workflow.root_entity_type = tracing::field::Empty,
        workflow.root_entity_id = tracing::field::Empty,
        workflow.run_id = tracing::field::Empty,
        temper.action = tracing::field::Empty,
        session.id = tracing::field::Empty,
        session_id = tracing::field::Empty,
        intent = tracing::field::Empty,
        observation_metadata = tracing::field::Empty,
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
        caller: WriteCaller,
        mut expected_authorization_precondition: Option<String>,
    ) -> Result<EntityResponse, DispatchError> {
        let params =
            match self.resolve_authenticated_params(tenant, entity_type, action, params, agent_ctx)
            {
                Ok(params) => params,
                Err(error) => {
                    self.stop_timer_no_one_can_run(tenant, action, agent_ctx, &error)
                        .await;
                    return Err(error);
                }
            };

        // The dispatch runs in the span it was called in: the request's, or the
        // root span of the runtime work that started it. Ids carried in the agent
        // context never re-parent or re-link it (see `trace_context`).
        // The timers this move sets keep whether its caller could read the
        // record now (`timer_standing`).
        let enriched_agent_ctx = self
            .with_read_when_set(
                tenant,
                entity_type,
                entity_id,
                action,
                keyed_for_move(
                    agent_ctx.for_dispatch_root(entity_type, entity_id),
                    tenant,
                    entity_type,
                    entity_id,
                    action,
                ),
            )
            .await?;
        let agent_ctx = &enriched_agent_ctx;
        record_workflow_span_attrs(agent_ctx, entity_type, entity_id, Some(action));
        let current_span = tracing::Span::current();
        current_span.record("session_id", agent_ctx.session_id.as_deref().unwrap_or(""));
        current_span.record("intent", agent_ctx.intent.as_deref().unwrap_or(""));
        let observation_metadata = agent_ctx.observation_metadata_json().unwrap_or_default();
        current_span.record("observation_metadata", observation_metadata.as_str());
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

        // Entry points retain their authorization boundary. Invalid first inputs
        // must not materialize an actor; existing actors validate hydrated state.
        let table = self.transition_table_for_dispatch(tenant, entity_type)?;
        if table.has_input_contracts() && !self.entity_exists(tenant, entity_type, entity_id) {
            let snapshot = self
                .load_authz_resource_snapshot(tenant, entity_type, entity_id)
                .await?;
            if !snapshot.exists {
                let response = snapshot.current_state;
                if let Err(error) = table.validate_action_params(
                    action,
                    &params,
                    &response.state.fields,
                    &response.state.counters,
                    &response.state.booleans,
                ) {
                    let mut response = response;
                    response.refusal = error.refusal;
                    response.refused = true;
                    let ctx = PostDispatchContext {
                        tenant,
                        entity_type,
                        entity_id,
                        action,
                        agent_ctx,
                        dispatch_idempotency_key: agent_ctx
                            .idempotency_key
                            .as_ref()
                            .map(crate::idempotency::IdempotencyKey::as_str),
                        action_params: &params,
                        await_integration,
                    };
                    return Ok(self
                        .refuse_before_actor(&ctx, response, error.message)
                        .await);
                }
            }
        }

        // W2 phase: actor_spawn — registry lookup / actor-creation path.
        // Emitted as a child span so `aggregate_spans group by resource_name`
        // slices dispatch latency by phase cleanly.
        let (actor_ref, actor_existed_before) = {
            let _phase = tracing::info_span!(
                "dispatch.phase.actor_spawn",
                tenant = %tenant,
                entity_type,
                entity_id,
            )
            .entered();
            let registry_start = std::time::Instant::now(); // determinism-ok: wall-clock latency metric only, not on simulation path
            let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
            let existed = self
                .actor_registry
                .read()
                .map(|reg| reg.contains_key(&actor_key))
                .unwrap_or(false);
            let Some(ar) = self.get_or_spawn_tenant_actor(tenant, entity_type, entity_id) else {
                return Err(DispatchError::Internal(format!(
                    "failed to resolve actor for governed entity type '{entity_type}'"
                )));
            };
            crate::runtime_metrics::record_actor_registry_lock_wait(
                entity_type,
                !existed,
                registry_start.elapsed(),
            );
            (ar, existed)
        };
        let cold_start_outcome_start = if !actor_existed_before {
            Some(std::time::Instant::now()) // determinism-ok: wall-clock latency metric only, not on simulation path
        } else {
            None
        };

        // Pre-resolve cross-entity state gates (Gap 1: Agent OS).
        let related = self
            .resolve_related_statuses(tenant, entity_type, entity_id, action)
            .await;

        // Relation integrity, field invariants and cross-invariants hold for the
        // record an outside call's action stores, and the actor is held to the
        // state that record was built from.
        let ctx = PostDispatchContext {
            tenant,
            entity_type,
            entity_id,
            action,
            agent_ctx,
            dispatch_idempotency_key: agent_ctx
                .idempotency_key
                .as_ref()
                .map(crate::idempotency::IdempotencyKey::as_str),
            action_params: &params,
            await_integration,
        };
        // Boxed so the preview's future does not deepen every caller's.
        if let Some(refused) = Box::pin(self.check_outside_action(
            &ctx,
            caller,
            &table,
            &related,
            &mut expected_authorization_precondition,
        ))
        .await?
        {
            return Ok(refused);
        }

        let action_params = params.clone();
        // ADR-0051: acquire an admission permit before spending retry budget.
        // Caps are pulled inline from the spec registry so `[admission]`
        // declarations take effect at spec load with no separate
        // registration step.
        // W2 phase: admission_acquire observability span (upstream main).
        let admission_span = tracing::info_span!(
            "dispatch.phase.admission_acquire",
            tenant = %tenant,
            entity_type,
            action_name = action,
        );
        let admission_start = std::time::Instant::now(); // determinism-ok: wall-clock latency metric only, not on simulation path
        let admission_caps = self.admission_caps_for(tenant, entity_type);
        let admission_was_capped = admission_caps.is_some();
        let admission_result = {
            use tracing::Instrument;
            self.admission
                .try_acquire_with_caps(tenant, entity_type, action, admission_caps.as_ref())
                .instrument(admission_span)
                .await
        };
        let _admission_permit = match admission_result {
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
        let cross_for_retry = related;
        let authorization_precondition_for_retry = expected_authorization_precondition;
        // The actor keeps the caller's identity on any scheduled action the
        // transition arms, so the action fires as them after a restart too,
        // and writes the deadline of each background call it starts.
        let mut actor_caller = agent_ctx.clone();
        actor_caller.background_calls = if await_integration {
            Vec::new()
        } else {
            self.background_calls(tenant, entity_type)
        };
        let actor_caller = Arc::new(actor_caller);
        let idempotency_key = agent_ctx.idempotency_key.clone();
        // W2 phase: ask_reply — round-trip through the retry layer to the
        // actor and back. Aggregates with other phase spans by prefix.
        let ask_span = tracing::info_span!(
            "dispatch.phase.ask_reply",
            tenant = %tenant,
            entity_type,
            action_name = %action_name,
        );
        let outcome = {
            use tracing::Instrument;
            let make_msg = || EntityMsg::Action {
                caller: Some(actor_caller.clone()),
                name: action_name.clone(),
                params: params_for_retry.clone(),
                related: cross_for_retry.clone(),
                idempotency_key: idempotency_key.clone(),
                expected_authorization_precondition: authorization_precondition_for_retry.clone(),
            };
            let is_timer_fire = agent_ctx
                .state_timer_armed_seq(tenant.as_str(), entity_type, entity_id, action)
                .is_some();
            async {
                if is_timer_fire {
                    // The timer owns this write and its gate slot. A request
                    // deadline cannot decide whether it committed or rolled back.
                    let start = std::time::Instant::now(); // determinism-ok: latency metric only
                    retry::AskResult {
                        result: actor_ref
                            .ask_until_reply::<EntityResponse>(make_msg())
                            .await,
                        attempts: 1,
                        elapsed: start.elapsed(),
                        retried_after_transient: false,
                    }
                } else {
                    retry::ask_with_backoff::<_, EntityResponse, _>(&actor_ref, make_msg, &policy)
                        .await
                }
            }
            .instrument(ask_span)
            .await
        };
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

        // W2 / temper#146: on a successful first-ask against a freshly
        // spawned actor, record the cold-start duration.
        if let Some(cold_start_begin) = cold_start_outcome_start
            && outcome.result.is_ok()
        {
            crate::runtime_metrics::record_actor_cold_start_duration(
                entity_type,
                cold_start_begin.elapsed(),
            );
        }

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
                    capture_seq: None,
                };
                let request_body_str = truncate_request_body_for_log(&action_params.to_string());
                let from_status = entry.from_status.as_deref().unwrap_or("unknown");
                let to_status = entry.to_status.as_deref().unwrap_or("unknown");
                let observation_metadata =
                    agent_ctx.observation_metadata_json().unwrap_or_default();
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
                    agent_id = entry.agent_id.as_deref().unwrap_or(""),
                    session_id = entry.session_id.as_deref().unwrap_or(""),
                    agent_type = entry.agent_type.as_deref().unwrap_or(""),
                    intent = entry.intent.as_deref().unwrap_or(""),
                    observation_metadata = %observation_metadata,
                    "app usage: {}.{} {} -> {} on {} failed",
                    entry.entity_type,
                    entry.action,
                    from_status,
                    to_status,
                    entry.entity_id
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
                self.persist_trajectory_entry_background(entry);
                return Err(DispatchError::from_actor_error(e, outcome.attempts));
            }
        };
        // A follow-up's retry answered by the outcome its key recorded is that
        // outcome from here on: its effects and reactions, not those of the
        // action this retry named.
        let answered_action = response.answered_action.clone();
        let action = answered_action.as_deref().unwrap_or(action);

        let stale = super::outside_write::stale_refusal(
            caller,
            authorization_precondition_for_retry.as_deref(),
            entity_type,
            entity_id,
            &response,
        );
        // Run all post-dispatch effects through the dedicated pipeline.
        let ctx = PostDispatchContext {
            tenant,
            entity_type,
            entity_id,
            action,
            agent_ctx,
            dispatch_idempotency_key: idempotency_key
                .as_ref()
                .map(crate::idempotency::IdempotencyKey::as_str),
            action_params: &action_params,
            await_integration,
        };
        // A successful reply's effects run once per key: a retry that reaches
        // the actor while another dispatcher runs them waits for it, and a
        // retry after they ran does not run them again. Either way the retry
        // answers at the state that dispatcher completed, never with the
        // actor's copy from before the effects ran.
        let mut response = match idempotency_key.as_ref() {
            Some(idem_key) if response.success => {
                let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
                match self
                    .idempotency_cache
                    .claim_effects(&actor_key, idem_key.as_str())
                    .await
                {
                    crate::idempotency::EffectsClaim::Applied(completed) => {
                        crate::idempotency::retry_reply(response, *completed)
                    }
                    crate::idempotency::EffectsClaim::Owner(owner) => {
                        let response = self.run_post_dispatch_effects(&ctx, response).await;
                        if response.success {
                            owner.complete(&idem_key.request(action, &action_params), &response);
                        }
                        response
                    }
                }
            }
            _ => self.run_post_dispatch_effects(&ctx, response).await,
        };
        // The reply the effects claim hands back may be another dispatcher's.
        response.answered_action = answered_action;
        if let Some(stale) = stale {
            tracing::Span::current().record("success", false);
            return Err(stale);
        }
        if response.success {
            self.clear_commons_storage_projection_cache_for_entity(entity_type);
        }

        tracing::Span::current().record("success", response.success);
        if let Some(ref err) = response.error {
            tracing::Span::current().record("error_msg", err.as_str());
        }

        Ok(response)
    }

    /// Answer a dispatch refused before it reached the actor. The refusal runs
    /// the post-dispatch pipeline like a refusal from the actor, so it is
    /// recorded the same way.
    pub(super) async fn refuse_before_actor(
        &self,
        ctx: &PostDispatchContext<'_>,
        mut response: EntityResponse,
        error: String,
    ) -> EntityResponse {
        response.success = false;
        response.refused = true;
        response.error = Some(error);
        let response = self.run_post_dispatch_effects(ctx, response).await;
        tracing::Span::current().record("success", false);
        if let Some(error) = &response.error {
            tracing::Span::current().record("error_msg", error.as_str());
        }
        response
    }
}

#[cfg(test)]
#[path = "actions_log_test.rs"]
mod log_truncation_tests;
