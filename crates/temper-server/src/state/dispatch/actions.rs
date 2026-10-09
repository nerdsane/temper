use tracing::instrument;
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::entity_actor::{EntityMsg, EntityResponse};
use crate::idempotency::{CompletionResult, OperationRequest};
use crate::request_context::remote_parent_context;
use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};
use temper_runtime::scheduler::{sim_now, sim_uuid};

use super::effects::PostDispatchContext;
use super::retry;
use super::{DispatchCommand, DispatchError, record_workflow_span_attrs};
use crate::state::admission::AdmissionOutcome;

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

impl crate::state::ServerState {
    /// Internal dispatch retains intentional get-or-create actor admission.
    pub(crate) async fn dispatch_tenant_action_with_completion(
        &self,
        cmd: DispatchCommand<'_>,
        expected_authorization_precondition: Option<String>,
        reaction_depth: Option<u32>,
    ) -> Result<CompletionResult, DispatchError> {
        self.dispatch_action_on_actor(
            cmd,
            expected_authorization_precondition,
            reaction_depth,
            None,
        )
        .await
    }

    /// Authorized actor dispatch followed by its registered completion scope.
    /// Historical replies never infer a fresh reaction obligation.
    #[instrument(skip_all, fields(
        otel.name = "dispatch.dispatch_tenant_action_core",
        tenant = %cmd.tenant,
        entity_type = cmd.entity_type,
        entity_id = cmd.entity_id,
        action_name = cmd.action,
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
    pub(super) async fn dispatch_action_on_actor(
        &self,
        cmd: DispatchCommand<'_>,
        expected_authorization_precondition: Option<String>,
        reaction_depth: Option<u32>,
        admitted_actor: Option<temper_runtime::actor::ActorRef<EntityMsg>>,
    ) -> Result<CompletionResult, DispatchError> {
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
        let params =
            self.resolve_authenticated_params(tenant, entity_type, action, params, agent_ctx)?;

        let explicit_workflow_context = agent_ctx.workflow_run_id.is_some()
            || agent_ctx.workflow_root_entity_type.is_some()
            || agent_ctx.workflow_root_entity_id.is_some();
        let use_workflow_root = crate::workflow_tracing::should_use_workflow_root_span(
            explicit_workflow_context,
            entity_type,
        );
        let mut enriched_agent_ctx = agent_ctx.for_dispatch_root(entity_type, entity_id);
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

        // Entry points retain their authorization boundary. Invalid first inputs
        // must not materialize an actor; existing actors validate hydrated state.
        let table = self.transition_table_for_dispatch(tenant, entity_type)?;
        if admitted_actor.is_none()
            && table.has_input_contracts()
            && !self.entity_exists(tenant, entity_type, entity_id)
        {
            let snapshot = self
                .load_authz_resource_snapshot(tenant, entity_type, entity_id)
                .await
                .map_err(DispatchError::Internal)?;
            if !snapshot.exists {
                let mut response = snapshot.current_state;
                if let Err(error) = table.validate_action_params(
                    action,
                    &params,
                    &response.state.fields,
                    &response.state.counters,
                    &response.state.booleans,
                ) {
                    response.success = false;
                    response.error = Some(error);
                    let ctx = PostDispatchContext {
                        tenant,
                        entity_type,
                        entity_id,
                        action,
                        agent_ctx,
                        dispatch_idempotency_key: agent_ctx.idempotency_key.as_deref(),
                        action_params: &params,
                        await_integration,
                    };
                    let response = self.run_post_dispatch_effects(&ctx, response).await;
                    tracing::Span::current().record("success", false);
                    if let Some(error) = &response.error {
                        tracing::Span::current().record("error_msg", error.as_str());
                    }
                    return Ok(CompletionResult::response(response));
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
            let Some(ar) = admitted_actor
                .or_else(|| self.get_or_spawn_tenant_actor(tenant, entity_type, entity_id))
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
            (ar, existed)
        };
        let cold_start_outcome_start = if !actor_existed_before {
            Some(std::time::Instant::now()) // determinism-ok: wall-clock latency metric only, not on simulation path
        } else {
            None
        };

        // Pre-resolve cross-entity state gates (Gap 1: Agent OS).
        let related = self
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
        let admission_permit = match admission_result {
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
        let outcome = {
            use tracing::Instrument;
            retry::ask_with_backoff::<_, EntityResponse, _>(
                &actor_ref,
                || EntityMsg::Action {
                    reply_mode: crate::idempotency::ActionReplyMode::Dispatch {
                        await_integration,
                        await_reactions,
                        reaction_depth,
                    },
                    name: action_name.clone(),
                    params: params_for_retry.clone(),
                    related: cross_for_retry.clone(),
                    idempotency_key: idempotency_key.clone(),
                    expected_authorization_precondition: authorization_precondition_for_retry
                        .clone(),
                },
                &policy,
            )
            .instrument(ask_span)
            .await
        };
        // Admission gates actor asks, not owned effects or descendant waits.
        drop(admission_permit);
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

        // ADR-0182: a mismatched or unverifiable idempotency key is a typed
        // client error. It committed nothing, so no post-dispatch effect runs.
        if !response.success
            && let Some(error) =
                DispatchError::from_idempotency_rejection(response.error.as_deref())
        {
            tracing::Span::current().record("success", false);
            tracing::Span::current().record("error_msg", error.to_string().as_str());
            return Err(error);
        }

        let result = self
            .complete_committed_operation(
                OperationRequest {
                    tenant: tenant.clone(),
                    entity_type: entity_type.into(),
                    entity_id: entity_id.into(),
                    action: action.into(),
                    params: action_params,
                    agent_ctx: agent_ctx.clone(),
                    idempotency_key,
                    await_integration,
                    reaction_depth,
                    detach_reactions: !await_reactions,
                },
                response,
                await_reactions,
            )
            .await;
        let response = &result.response;

        tracing::Span::current().record("success", response.success);
        if let Some(ref err) = response.error {
            tracing::Span::current().record("error_msg", err.as_str());
        }

        Ok(result)
    }
}

#[cfg(test)]
mod log_truncation_tests {
    use super::{LOG_REQUEST_BODY_MAX_BYTES, truncate_request_body_for_log};

    #[test]
    fn short_body_is_logged_verbatim() {
        let body = r#"{"ProductId":"p-1"}"#;
        assert_eq!(truncate_request_body_for_log(body), body);
    }

    #[test]
    fn oversized_ascii_body_is_marked_truncated() {
        let body = "a".repeat(LOG_REQUEST_BODY_MAX_BYTES + 100);
        let truncated = truncate_request_body_for_log(&body);
        assert!(truncated.ends_with("[truncated]"));
        assert_eq!(
            truncated.len(),
            LOG_REQUEST_BODY_MAX_BYTES + "[truncated]".len()
        );
    }

    #[test]
    fn oversized_multibyte_body_does_not_panic_at_the_cap() {
        // 3-byte characters: the cap at 4096 falls mid-character (4096 % 3 != 0),
        // which a raw byte slice would panic on.
        let body = "\u{4e16}".repeat(LOG_REQUEST_BODY_MAX_BYTES);
        let truncated = truncate_request_body_for_log(&body);
        assert!(truncated.ends_with("[truncated]"));
        assert!(truncated.len() <= LOG_REQUEST_BODY_MAX_BYTES + "[truncated]".len());
    }
}
