use super::failure_routing::{WasmFailure, failure_callback};
use super::{WasmDispatchCtx, WasmDispatchMode, WasmEntityRef, record_wasm_error_on_current_span};
use crate::entity_actor::EntityResponse;
use crate::request_context::AgentContext;
use crate::state::wasm_invocation_log::WasmInvocationEntry;
use temper_observe::wide_event;
use temper_runtime::scheduler::sim_now;
use tracing::{Instrument, instrument};

mod authorization;
mod awaited;
use awaited::{awaited_callback_failure_class, callback_agent_context};

fn persisted_authorization_reason(raw_reason: &str, typed_routes: bool) -> &str {
    if typed_routes {
        "AuthorizationDenied"
    } else {
        raw_reason
    }
}
impl crate::state::ServerState {
    /// Record a WASM invocation (persist log entry + emit observability events).
    #[expect(
        clippy::too_many_arguments,
        reason = "callback receipt fields remain explicit"
    )]
    pub(super) async fn record_invocation(
        &self,
        entity_ref: WasmEntityRef<'_>,
        module_name: &str,
        trigger_action: &str,
        callback_action: Option<String>,
        success: bool,
        error: Option<String>,
        duration_ms: u64,
        authz_denied: Option<bool>,
    ) {
        let log_entry = WasmInvocationEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: entity_ref.tenant.to_string(),
            entity_type: entity_ref.entity_type.to_string(),
            entity_id: entity_ref.entity_id.to_string(),
            module_name: module_name.to_string(),
            trigger_action: trigger_action.to_string(),
            callback_action,
            success,
            error: error.clone(),
            duration_ms,
            authz_denied,
        };
        let state = self.clone();
        let persist_entry = log_entry.clone();
        let span = tracing::info_span!(
            "dispatch.phase.persist_wasm_invocation",
            otel.name = "dispatch.phase.persist_wasm_invocation",
            tenant = %persist_entry.tenant,
            entity_type = %persist_entry.entity_type,
            entity_id = %persist_entry.entity_id,
            module_name = %persist_entry.module_name,
            trigger_action = %persist_entry.trigger_action,
            success = persist_entry.success,
        );
        tokio::spawn(
            // determinism-ok: background persist of WASM invocation
            async move {
                if let Err(e) = state.persist_wasm_invocation(&persist_entry).await {
                    tracing::error!(error = %e, "failed to persist WASM invocation");
                }
            }
            .instrument(span),
        );

        let wide = wide_event::from_wasm_invocation(wide_event::WasmInvocationInput {
            module_name,
            trigger_action,
            entity_type: entity_ref.entity_type,
            entity_id: entity_ref.entity_id,
            tenant: &entity_ref.tenant.to_string(),
            success,
            duration_ns: duration_ms * 1_000_000,
            error: error.as_deref(),
        });
        wide_event::emit_span(&wide);
        wide_event::emit_metrics(&wide);
    }

    #[instrument(skip_all, fields(
        otel.name = "dispatch.handle_wasm_failure",
        trigger_action,
        integration_name,
        module_name,
        error.type = tracing::field::Empty,
        failure.category = tracing::field::Empty,
        failure.code = tracing::field::Empty,
        error.message = tracing::field::Empty,
        exception.message = tracing::field::Empty,
    ))]
    pub(super) async fn handle_wasm_failure(
        &self,
        ctx: &WasmDispatchCtx<'_>,
        integration: &temper_spec::automaton::Integration,
        module_name: &str,
        failure: WasmFailure,
        duration_ms: u64,
    ) -> Result<Option<EntityResponse>, String> {
        let error_str = failure.diagnostic();
        let is_authz_denied = failure.is_authorization();
        let redact_guest_details = failure.has_guest_owned_content();
        let decision_id = if is_authz_denied {
            let persisted_reason = persisted_authorization_reason(
                error_str.as_str(),
                !integration.failure_routes.is_empty(),
            );
            self.record_wasm_authz_denial(
                ctx.entity_ref,
                ctx.action,
                &integration.name,
                module_name,
                persisted_reason,
                ctx.agent_ctx,
            )
        } else {
            None
        };

        if !integration.failure_routes.is_empty() {
            let envelope = match failure.into_envelope(
                ctx.dispatch_idempotency_key
                    .or(ctx.agent_ctx.idempotency_key.as_deref()),
                [
                    ctx.entity_ref.tenant.as_str(),
                    ctx.entity_ref.entity_type,
                    ctx.entity_ref.entity_id,
                    ctx.action,
                    &integration.name,
                ],
                decision_id.as_deref(),
            ) {
                Ok(envelope) => envelope,
                Err(error) => {
                    self.complete_awaited_module_failure(
                        ctx.dispatch_idempotency_key,
                        ctx.agent_ctx,
                        None,
                        None,
                    )
                    .await?;
                    return Err(format!("InvalidFailureAdapterOutput: {error}"));
                }
            };
            self.record_typed_failure_observation(
                ctx.entity_ref,
                &integration.name,
                ctx.action,
                &envelope,
                redact_guest_details,
            );
            let callback_result = failure_callback(integration, envelope.category);
            self.record_invocation(
                ctx.entity_ref,
                module_name,
                ctx.action,
                callback_result
                    .as_ref()
                    .ok()
                    .map(|callback| callback.to_string()),
                false,
                Some(format!(
                    "typed failure category={:?} code={}",
                    envelope.category,
                    envelope.code.as_str()
                )),
                duration_ms,
                is_authz_denied.then_some(true),
            )
            .await;
            let params = serde_json::json!({"failure": envelope});
            let callback = self
                .settle_awaited_typed_failure(
                    ctx.dispatch_idempotency_key,
                    ctx.agent_ctx,
                    callback_result,
                    params.clone(),
                )
                .await?;
            let callback_ctx = super::super::typed_failure::typed_failure_callback_context(
                ctx.agent_ctx,
                &envelope.operation.id,
                callback,
            );
            return super::dispatch_wasm_callback_boxed(
                self,
                ctx.entity_ref,
                callback,
                params,
                &callback_ctx,
                Some(ctx.agent_ctx),
                ctx.mode,
                &integration.name,
                module_name,
                true,
            )
            .await;
        }

        record_wasm_error_on_current_span(&error_str);
        self.record_invocation(
            ctx.entity_ref,
            module_name,
            ctx.action,
            integration.on_failure.clone(),
            false,
            Some(error_str.clone()),
            duration_ms,
            is_authz_denied.then_some(true),
        )
        .await;

        if let Some(cb) = &integration.on_failure {
            let mut params = serde_json::json!({
                "error": error_str.clone(),
                "error_message": error_str,
                "integration": integration.name,
            });
            if let Some(ref did) = decision_id {
                params["decision_id"] = serde_json::json!(did);
                params["authz_denied"] = serde_json::json!(true);
            }
            self.complete_awaited_module_failure(
                ctx.dispatch_idempotency_key,
                ctx.agent_ctx,
                Some(cb),
                Some(params.clone()),
            )
            .await?;
            let response = super::dispatch_wasm_callback_boxed(
                self,
                ctx.entity_ref,
                cb,
                params,
                ctx.agent_ctx,
                None,
                ctx.mode,
                &integration.name,
                module_name,
                false,
            )
            .await?;
            return Ok(response);
        }

        self.complete_awaited_module_failure(
            ctx.dispatch_idempotency_key,
            ctx.agent_ctx,
            None,
            None,
        )
        .await?;

        // No declared recovery: propagate the failure instead of swallowing it
        // (ADR-0152). The invocation was already recorded above, so telemetry
        // is preserved. Inline this surfaces as `success: false`; background
        // the dispatcher drives a compensating transition.
        Err(error_str)
    }

    #[instrument(skip_all, fields(otel.name = "dispatch.dispatch_wasm_callback", callback_action))]
    #[expect(
        clippy::too_many_arguments,
        reason = "callback dispatch preserves separate authorities"
    )]
    pub(super) async fn dispatch_wasm_callback(
        &self,
        entity_ref: WasmEntityRef<'_>,
        callback_action: &str,
        callback_params: serde_json::Value,
        agent_ctx: &AgentContext,
        awaited_agent_ctx: Option<&AgentContext>,
        mode: WasmDispatchMode,
        integration_name: &str,
        module_name: &str,
        preserve_idempotency: bool,
    ) -> Result<Option<EntityResponse>, String> {
        let awaited_agent_ctx = awaited_agent_ctx.unwrap_or(agent_ctx);
        match mode {
            WasmDispatchMode::Inline => {
                // Preserve inline semantics through nested WASM callbacks.
                // A public action may dispatch a validation callback that has
                // its own WASM trigger; returning before that nested trigger
                // commits lets concurrent requests observe stale detailed
                // fields while counters advance.
                let callback_ctx = if preserve_idempotency {
                    agent_ctx.clone()
                } else {
                    callback_agent_context(
                        agent_ctx,
                        integration_name,
                        module_name,
                        callback_action,
                    )
                };
                let reaction_context = self
                    .awaited_callback_commit_context(
                        entity_ref,
                        callback_action,
                        &callback_params,
                        awaited_agent_ctx,
                    )
                    .await?;
                let dispatch = super::dispatch_tenant_action_core_boxed(
                    self,
                    entity_ref.tenant,
                    entity_ref.entity_type,
                    entity_ref.entity_id,
                    callback_action,
                    callback_params,
                    &callback_ctx,
                    true,
                    reaction_context,
                    None,
                )
                .await;
                let awaited_owner =
                    awaited_agent_ctx
                        .idempotency_key
                        .as_deref()
                        .and_then(|delivery_id| {
                            self.awaited_execution_owner(delivery_id, awaited_agent_ctx)
                        });
                let resp = match dispatch {
                    Ok(response) => response,
                    Err(error) => {
                        if let Some(owner) = awaited_owner.as_ref()
                            && let Err(evidence_error) = owner
                                .record_callback_failure(
                                    awaited_callback_failure_class(&error),
                                    sim_now(),
                                )
                                .await
                        {
                            tracing::error!(
                                callback = callback_action,
                                error = %evidence_error,
                                "failed to persist awaited callback failure evidence"
                            );
                        }
                        return Err(error.to_string());
                    }
                };
                if !resp.success
                    && let Some(owner) = awaited_owner
                    && let Err(evidence_error) = owner
                        .record_callback_failure(
                            crate::trigger::delivery::AwaitedExecutionFailureClass::CallbackRejected,
                            sim_now(),
                        )
                        .await
                {
                    tracing::error!(
                        callback = callback_action,
                        error = %evidence_error,
                        "failed to persist awaited callback rejection evidence"
                    );
                }
                Ok(Some(resp))
            }
            WasmDispatchMode::Background => {
                let callback_ctx = super::super::typed_failure::background_callback_context(
                    "wasm-runtime",
                    agent_ctx,
                    preserve_idempotency,
                );
                self.dispatch_tenant_action(
                    entity_ref.tenant,
                    entity_ref.entity_type,
                    entity_ref.entity_id,
                    callback_action,
                    callback_params,
                    &callback_ctx,
                )
                .await
                .map_err(|e| {
                    let msg = format!("failed to dispatch WASM callback '{callback_action}': {e}");
                    tracing::error!(callback = %callback_action, error = %e, "{msg}");
                    msg
                })?;
                Ok(None)
            }
        }
    }
}

#[cfg(test)]
mod tests;
