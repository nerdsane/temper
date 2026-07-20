//! Actor-backed core dispatch implementation.

use super::*;

impl crate::state::ServerState {
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
    pub(super) async fn dispatch_tenant_action_core_inner(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        action: &str,
        params: serde_json::Value,
        agent_ctx: &AgentContext,
        await_integration: bool,
        timeout_precondition: Option<StateTimeoutPrecondition>,
    ) -> Result<EntityResponse, DispatchError> {
        let is_state_timeout_dispatch = timeout_precondition.is_some();
        let explicit_workflow_context = agent_ctx.workflow_run_id.is_some()
            || agent_ctx.workflow_root_entity_type.is_some()
            || agent_ctx.workflow_root_entity_id.is_some();
        let use_workflow_root = crate::workflow_tracing::should_use_workflow_root_span(
            explicit_workflow_context,
            entity_type,
        );
        let mut enriched_agent_ctx = agent_ctx.for_dispatch_root(entity_type, entity_id);
        let mut workflow_root_active = false;
        if use_workflow_root {
            let root_entity_type = enriched_agent_ctx
                .workflow_root_entity_type
                .as_deref()
                .unwrap_or(entity_type);
            let root_entity_id = enriched_agent_ctx
                .workflow_root_entity_id
                .as_deref()
                .unwrap_or(entity_id);
            let workflow_run_id = enriched_agent_ctx
                .workflow_run_id
                .as_deref()
                .unwrap_or(root_entity_id);
            if let Some(parent) = self.workflow_spans.parent_context(
                tenant.as_str(),
                root_entity_type,
                root_entity_id,
                workflow_run_id,
                enriched_agent_ctx.session_id.as_deref(),
            ) {
                workflow_root_active = true;
                tracing::Span::current().set_parent(parent);
            }
        } else if let Some(parent) = remote_parent_context(agent_ctx) {
            tracing::Span::current().set_parent(parent);
        }
        enriched_agent_ctx = enriched_agent_ctx.with_current_span_trace_context();
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

        // W2 phase: actor_spawn — registry lookup / actor-creation path.
        // Emitted as a child span so `aggregate_spans group by resource_name`
        // slices dispatch latency by phase cleanly.
        let actor_spawn_span = tracing::info_span!(
            "dispatch.phase.actor_spawn",
            tenant = %tenant,
            entity_type,
            entity_id,
        );
        let actor_resolution = async {
            let registry_start = std::time::Instant::now(); // determinism-ok: wall-clock latency metric only, not on simulation path
            let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
            let existed = self
                .actor_registry
                .read()
                .map(|reg| {
                    reg.get(&actor_key).is_some_and(|actor_ref| {
                        !actor_ref.is_draining() && !actor_ref.is_stopped()
                    })
                })
                .unwrap_or(false);
            let Some(ar) = self
                .get_or_spawn_tenant_actor_when_ready(tenant, entity_type, entity_id)
                .await
            else {
                return Err(DispatchError::Internal(format!(
                    "failed to resolve actor for governed entity type '{entity_type}'"
                )));
            };
            crate::runtime_metrics::record_actor_registry_lock_wait(
                entity_type,
                !existed,
                registry_start.elapsed(),
            );
            Ok((ar, existed))
        }
        .instrument(actor_spawn_span)
        .await;
        let (actor_ref, actor_existed_before) = match actor_resolution {
            Ok(resolved) => resolved,
            Err(error) => {
                if is_state_timeout_dispatch
                    && let Some(cancellation) = self
                        .recover_deleted_state_timeout_cancellation(tenant, entity_type, entity_id)
                        .await
                        .map_err(DispatchError::Internal)?
                {
                    return Ok(cancellation);
                }
                return Err(error);
            }
        };
        let cold_start_outcome_start = if !actor_existed_before {
            Some(std::time::Instant::now()) // determinism-ok: wall-clock latency metric only, not on simulation path
        } else {
            None
        };

        // Pre-resolve cross-entity state gates (Gap 1: Agent OS).
        let cross_entity_booleans = self
            .resolve_cross_entity_guards(tenant, entity_type, entity_id, action)
            .await;

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
        let cross_for_retry = cross_entity_booleans;
        let idempotency_key = Some(agent_ctx.idempotency_key.clone().unwrap_or_else(|| {
            format!(
                "dispatch:{tenant}:{entity_type}:{entity_id}:{action}:{}",
                sim_uuid()
            )
        }));
        // W2 phase: ask_reply — round-trip through the retry layer to the
        // actor and back. Aggregates with other phase spans by prefix.
        let ask_span = tracing::info_span!(
            "dispatch.phase.ask_reply",
            tenant = %tenant,
            entity_type,
            action_name = %action_name,
        );
        let (actor_ref, outcome) = {
            use tracing::Instrument;
            self.ask_actor_with_drain_retry::<EntityResponse, _>(
                tenant,
                entity_type,
                entity_id,
                actor_ref,
                || EntityMsg::Action {
                    name: action_name.clone(),
                    params: params_for_retry.clone(),
                    cross_entity_booleans: cross_for_retry.clone(),
                    idempotency_key: idempotency_key.clone(),
                    state_timeout_precondition: timeout_precondition.clone().map(Box::new),
                },
                &policy,
            )
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
                };
                let request_body_str = {
                    let s = action_params.to_string();
                    if s.len() > 4096 {
                        format!("{}[truncated]", &s[..4096])
                    } else {
                        s
                    }
                };
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

        let ctx = PostDispatchContext {
            tenant,
            entity_type,
            entity_id,
            action,
            agent_ctx,
            dispatch_idempotency_key: idempotency_key.as_deref(),
            action_params: &action_params,
            await_integration,
            actor_uid: Some(actor_ref.id().uid),
        };

        // A stale state-timeout condition is an expected cancellation, not a
        // failed user intent. Its response is nevertheless the authoritative
        // actor state and may be the first time this process observes a reset
        // committed by another replica. Reconcile only timeout ownership from
        // it; keep the cancelled action out of trajectories, integrations,
        // projections, reactions, and other post-dispatch effects.
        if is_state_timeout_cancellation(is_state_timeout_dispatch, response.error.as_deref()) {
            let inactive_timeout_fence = if response.state.status == "Deleted" {
                self.reconcile_state_timeout_after_synthetic_commit(
                    tenant,
                    entity_type,
                    entity_id,
                    &response.state,
                )
            } else {
                self.arm_state_timeouts_if_needed(&ctx, &response);
                None
            };
            if response.state.status == "Deleted"
                && self
                    .stop_and_remove_entity_if_current(
                        tenant,
                        entity_type,
                        entity_id,
                        actor_ref.id().uid,
                    )
                    .await
            {
                self.release_inactive_state_timeout_after_actor_eviction(
                    tenant,
                    entity_type,
                    entity_id,
                    inactive_timeout_fence,
                );
            }
            return Ok(response);
        }

        // Inline WASM and adapter callbacks re-enter core dispatch while the
        // outer effects pipeline is still awaited. Erasing this future keeps
        // each nested effects state machine heap-backed instead of embedding
        // another copy in its caller, bounding the poll stack without changing
        // the awaited ordering.
        let post_dispatch: std::pin::Pin<
            Box<dyn std::future::Future<Output = EntityResponse> + Send + '_>,
        > = Box::pin(self.run_post_dispatch_effects(&ctx, response));
        let response = post_dispatch.await;
        if response.success
            && let Some(ref idem_key) = idempotency_key
        {
            let actor_key = format!("{tenant}:{entity_type}:{entity_id}");
            self.idempotency_cache
                .mark_effects_applied(&actor_key, idem_key);
        }
        if response.success {
            self.clear_commons_storage_projection_cache_for_entity(entity_type);
        }

        tracing::Span::current().record("success", response.success);
        if let Some(ref err) = response.error {
            tracing::Span::current().record("error_msg", err.as_str());
        }
        if workflow_root_active && let Some(workflow_run_id) = agent_ctx.workflow_run_id.clone() {
            self.workflow_spans.finish_if_terminal_after_drain(
                workflow_run_id,
                entity_type.to_string(),
                entity_id.to_string(),
                response.state.status.clone(),
            );
        }

        Ok(response)
    }
}
