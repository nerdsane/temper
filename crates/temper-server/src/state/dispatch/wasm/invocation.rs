//! WASM engine invocation and result handling.

use super::*;

impl crate::state::ServerState {
    /// Invoke the WASM module and handle success/failure/error results.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn invoke_and_handle_result(
        &self,
        ctx: &WasmDispatchCtx<'_>,
        integration: &temper_spec::automaton::Integration,
        module_name: &str,
        hash: &str,
        entity_state: &EntityState,
        inv_ctx: WasmInvocationContext,
        host: Arc<dyn WasmHost>,
        limits: &WasmResourceLimits,
        denial_tracker: &HttpCallAuthzDenialTracker,
        blob_cache: std::collections::BTreeMap<String, Vec<u8>>,
        llm_parent_span_id: Option<&str>,
    ) -> Result<Option<EntityResponse>, String> {
        // Existing action-triggered invocations don't use streams — pass empty registry.
        let streams = Arc::new(std::sync::RwLock::new(StreamRegistry::default()));
        let phase_parent_span = Span::current();
        let invoke_result = instrument_wasm_dispatch_phase_result(
            phase_parent_span.clone(),
            ctx,
            module_name,
            WASM_DISPATCH_PHASE_ENGINE_INVOKE,
            self.wasm_engine
                .invoke_with_blobs(hash, &inv_ctx, host, limits, streams, blob_cache),
        )
        .await;
        match invoke_result {
            Ok(mut result) if result.success => {
                if integration.llm {
                    let session_id = ctx
                        .agent_ctx
                        .session_id
                        .as_deref()
                        .unwrap_or(ctx.entity_ref.entity_id);
                    attach_llm_parent_context(
                        &Span::current(),
                        llm_parent_span_id,
                        entity_state,
                        session_id,
                        result.duration_ms,
                        &mut result.callback_params,
                    );
                }

                let callback_params = &result.callback_params;

                if should_record_gen_ai_span_attrs(integration.llm, callback_params) {
                    // Record GenAI token usage from callback params (if present)
                    if let Some(input) =
                        callback_params.get("input_tokens").and_then(|v| v.as_i64())
                    {
                        Span::current().record("gen_ai.usage.input_tokens", input);
                    }
                    if let Some(output) = callback_params
                        .get("output_tokens")
                        .and_then(|v| v.as_i64())
                    {
                        Span::current().record("gen_ai.usage.output_tokens", output);
                    }
                    // Record GenAI input/output messages for LLM Observability content.
                    // These are JSON strings of message arrays set by WASM modules.
                    if let Some(input_msgs) = callback_params
                        .get("_gen_ai_input_messages")
                        .and_then(|v| v.as_str())
                    {
                        Span::current().record("gen_ai.input.messages", input_msgs);
                    }
                    if let Some(output_msgs) = callback_params
                        .get("_gen_ai_output_messages")
                        .and_then(|v| v.as_str())
                    {
                        Span::current().record("gen_ai.output.messages", output_msgs);
                    }
                    if let Some(system_instructions) = callback_params
                        .get("_gen_ai_system_instructions")
                        .and_then(|v| v.as_str())
                        .filter(|value| !value.is_empty())
                    {
                        Span::current().record("gen_ai.system_instructions", system_instructions);
                    }
                    let provider = llm_provider_for_observability(entity_state, callback_params);
                    Span::current().record("gen_ai.system", provider.as_str());
                    Span::current().record("gen_ai.provider.name", provider.as_str());
                    let model = llm_model_for_observability(entity_state, callback_params);
                    Span::current().record("gen_ai.request.model", model.as_str());
                    if let Some(finish_reason) = callback_params
                        .get("_gen_ai_finish_reason")
                        .and_then(|v| v.as_str())
                        .filter(|value| !value.is_empty())
                    {
                        Span::current().record("gen_ai.response.finish_reasons", finish_reason);
                    }
                }

                with_wasm_dispatch_phase(
                    &phase_parent_span,
                    ctx,
                    module_name,
                    WASM_DISPATCH_PHASE_RESULT_OBSERVE_COMPLETE,
                    || {
                        let complete_seq = self.next_entity_event_sequence(
                            ctx.entity_ref.tenant.as_str(),
                            ctx.entity_ref.entity_type,
                            ctx.entity_ref.entity_id,
                        );
                        self.record_entity_observe_event_with_seq(
                            ctx.entity_ref.tenant.as_str(),
                            ctx.entity_ref.entity_type,
                            ctx.entity_ref.entity_id,
                            complete_seq,
                            "integration_complete",
                            serde_json::json!({
                                "seq": complete_seq,
                                "integration": integration.name,
                                "module": module_name,
                                "trigger_action": ctx.action,
                                "result": "success",
                                "callback_action": result.callback_action.clone(),
                                "duration_ms": result.duration_ms,
                            }),
                        );
                    },
                );
                if let Some(reason) = denial_tracker.take_denial() {
                    let error_str = http_call_authz_denied_error(&reason);
                    record_wasm_error_on_current_span(&error_str);
                    return self
                        .handle_wasm_failure(
                            ctx,
                            &integration.name,
                            module_name,
                            &integration.on_failure,
                            error_str,
                            result.duration_ms,
                        )
                        .await;
                }

                if integration.llm {
                    instrument_wasm_dispatch_phase(
                        phase_parent_span.clone(),
                        ctx,
                        module_name,
                        WASM_DISPATCH_PHASE_LLMOBS_SUBMIT,
                        async {
                            let event = llm_call_wide_event(
                                ctx,
                                entity_state,
                                callback_params,
                                result.duration_ms,
                            );
                            temper_observe::wide_event::emit_span(&event);
                            temper_observe::wide_event::emit_metrics(&event);
                            submit_llmobs_llm_span(
                                ctx,
                                entity_state,
                                callback_params,
                                result.duration_ms,
                                module_name,
                            )
                            .await;
                        },
                    )
                    .await;
                }
                if module_name == MONTY_REPL_MODULE {
                    instrument_wasm_dispatch_phase(
                        phase_parent_span.clone(),
                        ctx,
                        module_name,
                        WASM_DISPATCH_PHASE_LLMOBS_SUBMIT,
                        submit_llmobs_tool_spans(ctx, entity_state, callback_params),
                    )
                    .await;
                }

                instrument_wasm_dispatch_phase(
                    phase_parent_span.clone(),
                    ctx,
                    module_name,
                    WASM_DISPATCH_PHASE_RECORD_INVOCATION,
                    self.record_invocation(
                        ctx.entity_ref,
                        module_name,
                        ctx.action,
                        Some(result.callback_action.clone()),
                        true,
                        None,
                        result.duration_ms,
                        None,
                    ),
                )
                .await;

                let callback_params = strip_private_observability_params(result.callback_params);
                let composite_agent_ctx = agent_ctx_for_composite_wasm_result(
                    ctx.agent_ctx,
                    ctx.dispatch_idempotency_key,
                );
                let composite_result_consumed = self
                    .apply_composite_integration_result(
                        ctx.entity_ref.tenant,
                        ctx.entity_ref.entity_type,
                        ctx.entity_ref.entity_id,
                        ctx.action,
                        &callback_params,
                        &composite_agent_ctx,
                    )
                    .await
                    .map_err(|e| e.to_string())?;

                // Determine callback action: prefer static on_success from spec,
                // fall back to dynamic callback_action from WASM result. Composite
                // integrations may return only a data envelope for the kernel to
                // apply; the default SDK "callback" action should not become an
                // implicit source-entity dispatch in that path.
                let mut callback_action = integration
                    .on_success
                    .as_deref()
                    .unwrap_or(&result.callback_action);
                if composite_result_consumed
                    && integration.on_success.is_none()
                    && result.callback_action == "callback"
                {
                    callback_action = "";
                }

                if !callback_action.is_empty() {
                    let callback_response = instrument_wasm_dispatch_phase_result(
                        phase_parent_span.clone(),
                        ctx,
                        module_name,
                        WASM_DISPATCH_PHASE_DISPATCH_CALLBACK,
                        self.dispatch_wasm_callback(
                            ctx.entity_ref,
                            callback_action,
                            callback_params,
                            ctx.agent_ctx,
                            ctx.mode,
                        ),
                    )
                    .await?;
                    if let Some(resp) = callback_response {
                        return Ok(Some(resp));
                    }
                }
                Ok(None)
            }
            Ok(result) => {
                with_wasm_dispatch_phase(
                    &phase_parent_span,
                    ctx,
                    module_name,
                    WASM_DISPATCH_PHASE_RESULT_OBSERVE_COMPLETE,
                    || {
                        let complete_seq = self.next_entity_event_sequence(
                            ctx.entity_ref.tenant.as_str(),
                            ctx.entity_ref.entity_type,
                            ctx.entity_ref.entity_id,
                        );
                        self.record_entity_observe_event_with_seq(
                            ctx.entity_ref.tenant.as_str(),
                            ctx.entity_ref.entity_type,
                            ctx.entity_ref.entity_id,
                            complete_seq,
                            "integration_complete",
                            serde_json::json!({
                                "seq": complete_seq,
                                "integration": integration.name,
                                "module": module_name,
                                "trigger_action": ctx.action,
                                "result": "failure",
                                "callback_action": result.callback_action.clone(),
                                "duration_ms": result.duration_ms,
                                "error": result.error.clone(),
                            }),
                        );
                    },
                );
                let mut error_str = result.error.unwrap_or_else(|| {
                    format!(
                        "WASM integration '{}' returned unsuccessful result",
                        integration.name
                    )
                });
                if let Some(reason) = denial_tracker.take_denial() {
                    error_str = http_call_authz_denied_error(&reason);
                }
                record_wasm_error_on_current_span(&error_str);
                // A failed integration's effect never landed. `handle_wasm_failure`
                // records the invocation, then either runs the declared
                // `on_failure` recovery or — when none is declared — returns
                // `Err` so the failure is never silently treated as success
                // (ADR-0152).
                self.handle_wasm_failure(
                    ctx,
                    &integration.name,
                    module_name,
                    &integration.on_failure,
                    error_str,
                    result.duration_ms,
                )
                .await
            }
            Err(e) => {
                with_wasm_dispatch_phase(
                    &phase_parent_span,
                    ctx,
                    module_name,
                    WASM_DISPATCH_PHASE_RESULT_OBSERVE_COMPLETE,
                    || {
                        let complete_seq = self.next_entity_event_sequence(
                            ctx.entity_ref.tenant.as_str(),
                            ctx.entity_ref.entity_type,
                            ctx.entity_ref.entity_id,
                        );
                        self.record_entity_observe_event_with_seq(
                            ctx.entity_ref.tenant.as_str(),
                            ctx.entity_ref.entity_type,
                            ctx.entity_ref.entity_id,
                            complete_seq,
                            "integration_complete",
                            serde_json::json!({
                                "seq": complete_seq,
                                "integration": integration.name,
                                "module": module_name,
                                "trigger_action": ctx.action,
                                "result": "error",
                                "duration_ms": 0,
                                "error": e.to_string(),
                            }),
                        );
                    },
                );
                let mut error_str = e.to_string();
                if let Some(reason) = denial_tracker.take_denial()
                    && !is_http_call_authz_denial(&error_str)
                {
                    error_str = http_call_authz_denied_error(&reason);
                }
                record_wasm_error_on_current_span(&error_str);
                // Same as the unsuccessful-result arm above: a host trap, fuel
                // exhaustion, or panic also leaves the integration's effect
                // unrealized. `handle_wasm_failure` records it and propagates
                // `Err` when no `on_failure` is declared (ADR-0152).
                self.handle_wasm_failure(
                    ctx,
                    &integration.name,
                    module_name,
                    &integration.on_failure,
                    error_str,
                    0,
                )
                .await
            }
        }
    }

    /// Invoke a WASM module directly (not triggered by an entity action).
    ///
    /// Used by `$value` handlers for blob operations. The WASM module controls
    /// the entire blob lifecycle (auth, hashing, caching, upload/download) via
    /// streaming host functions. Bytes never enter WASM memory.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn invoke_wasm_direct(
        &self,
        tenant: &TenantId,
        module_name: &str,
        mut context: WasmInvocationContext,
        streams: Arc<std::sync::RwLock<StreamRegistry>>,
    ) -> Result<temper_wasm::WasmInvocationResult, String> {
        if context.wasm_module.is_none() {
            context.wasm_module = Some(module_name.to_string());
        }
        // Resolve module hash
        let module_hash = {
            let wasm_reg = self.wasm_module_registry.read().unwrap(); // ci-ok: infallible lock
            wasm_reg
                .get_hash(tenant, module_name)
                .map(|s| s.to_string())
        };
        let hash = module_hash.ok_or_else(|| {
            format!("WASM module '{module_name}' not found for tenant '{tenant}'")
        })?;
        self.ensure_wasm_module_cached(tenant, module_name, &hash)
            .await?;

        // Build authorized host chain
        let base_gate = self.wasm_authz_gate();
        let authz_ctx = WasmAuthzContext {
            tenant: tenant.to_string(),
            module_name: module_name.to_string(),
            agent_id: context.agent_id.clone(),
            session_id: context.session_id.clone(),
            entity_type: context.entity_type.clone(),
            trigger_action: context.trigger_action.clone(),
        };
        let tenant_secrets =
            self.get_authorized_wasm_host_bootstrap_secrets(tenant, &*base_gate, &authz_ctx);
        let secret_resolver =
            self.authorized_wasm_secret_resolver(tenant, Arc::clone(&base_gate), authz_ctx.clone());
        let local_blob_interceptor = local_blob_binary_interceptor(
            self.clone(),
            tenant.clone(),
            tenant_secrets.get("blob_endpoint").cloned(),
        );
        let progress_emitter = progress_emitter_fn(
            self.clone(),
            tenant.to_string(),
            context.entity_type.clone(),
            context.entity_id.clone(),
            module_name.to_string(),
        );
        let mut base_host = ProductionWasmHost::new(tenant_secrets)
            .with_spec_evaluator(spec_evaluator_fn())
            .with_progress_emitter(progress_emitter)
            .with_internal_api_base_url(internal_api_base_url(self))
            .with_internal_api_key(std::env::var("TEMPER_API_KEY").ok()) // determinism-ok: production host loopback config
            .with_invocation_context(context.clone());
        if let Some(resolver) = secret_resolver {
            base_host = base_host.with_secret_resolver(resolver);
        }
        if let Some(interceptor) = local_blob_interceptor {
            base_host = base_host.with_binary_http_interceptor(interceptor);
        }
        let production_host: Arc<dyn WasmHost> = Arc::new(base_host);
        let inner: Arc<dyn WasmHost> = Arc::new(LocalTDataWasmHost::new(
            self.clone(),
            tenant.clone(),
            None,
            production_host,
        ));
        let host: Arc<dyn WasmHost> =
            Arc::new(AuthorizedWasmHost::new(inner, base_gate, authz_ctx));
        let limits = WasmResourceLimits::default();

        tracing::info!(
            tenant = %tenant,
            module = %module_name,
            hash = %hash,
            trigger = %context.trigger_action,
            "invoking WASM module directly for $value"
        );

        self.wasm_engine
            .invoke(&hash, &context, host, &limits, streams)
            .await
            .map_err(|e| e.to_string())
    }
}
