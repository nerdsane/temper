//! Integration selection and single-integration dispatch.

use super::*;

impl crate::state::ServerState {
    #[instrument(skip_all, fields(
        otel.name = %format_args!("{}.{}.integrations", req.entity_type, req.action),
        tenant = %req.tenant,
        entity_type = req.entity_type,
        entity_id = req.entity_id,
        action_name = req.action,
        workflow.root_entity_type = tracing::field::Empty,
        workflow.root_entity_id = tracing::field::Empty,
        workflow.run_id = tracing::field::Empty,
        temper.action = tracing::field::Empty,
        session.id = tracing::field::Empty,
    ))]
    pub(crate) async fn dispatch_wasm_integrations_internal(
        &self,
        req: &WasmDispatchRequest<'_>,
    ) -> Result<Option<EntityResponse>, String> {
        record_workflow_span_attrs(
            req.agent_ctx,
            req.entity_type,
            req.entity_id,
            Some(req.action),
        );
        let integrations = {
            let registry = self.registry.read().unwrap(); // ci-ok: infallible lock
            registry
                .get_spec(req.tenant, req.entity_type)
                .map(|spec| spec.integrations.clone())
                .unwrap_or_default()
        };
        let base_gate = self.wasm_authz_gate();
        let ctx = WasmDispatchCtx {
            entity_ref: WasmEntityRef {
                tenant: req.tenant,
                entity_type: req.entity_type,
                entity_id: req.entity_id,
            },
            action: req.action,
            agent_ctx: req.agent_ctx,
            dispatch_idempotency_key: req.dispatch_idempotency_key,
            mode: req.mode,
        };
        let mut last_response: Option<EntityResponse> = None;

        for effect_name in req.custom_effects {
            let integration = integrations
                .iter()
                .find(|ig| ig.integration_type == "wasm" && ig.trigger == *effect_name)
                .cloned();
            let Some(integration) = integration else {
                continue;
            };

            // The single-integration future contains the full invocation and
            // callback pipeline. Keep that state heap-backed so a blocking
            // callback cannot exhaust the runtime worker's stack in debug
            // builds while nested dispatch is being polled.
            if let Some(resp) = Box::pin(self.dispatch_single_integration(
                &ctx,
                &integration,
                req.entity_state,
                req.action_params,
                &base_gate,
            ))
            .await?
            {
                last_response = Some(resp);
            }
        }

        Ok(last_response)
    }

    /// Dispatch a single WASM integration: resolve module, invoke, handle result.
    #[instrument(skip_all, fields(
        otel.name = tracing::field::Empty,
        integration = %integration.name,
        wasm.module = tracing::field::Empty,
        wasm.timeout_source = tracing::field::Empty,
        gen_ai.system = tracing::field::Empty,
        gen_ai.provider.name = tracing::field::Empty,
        gen_ai.system_instructions = tracing::field::Empty,
        gen_ai.request.model = tracing::field::Empty,
        gen_ai.operation.name = tracing::field::Empty,
        gen_ai.response.finish_reasons = tracing::field::Empty,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.conversation.id = tracing::field::Empty,
        gen_ai.input.messages = tracing::field::Empty,
        gen_ai.output.messages = tracing::field::Empty,
        error.type = tracing::field::Empty,
        error.message = tracing::field::Empty,
        exception.message = tracing::field::Empty,
    ))]
    pub(super) async fn dispatch_single_integration(
        &self,
        ctx: &WasmDispatchCtx<'_>,
        integration: &temper_spec::automaton::Integration,
        entity_state: &EntityState,
        action_params: &serde_json::Value,
        base_gate: &Arc<dyn WasmAuthzGate>,
    ) -> Result<Option<EntityResponse>, String> {
        // --- Resolve module ---
        let Some(module_name) = integration.module.clone() else {
            tracing::warn!(
                tenant = %ctx.entity_ref.tenant,
                entity_type = ctx.entity_ref.entity_type,
                integration = %integration.name,
                "WASM integration missing module name"
            );
            return Ok(None);
        };

        let current_span = Span::current();
        let llm_parent_span_id = if integration.llm {
            current_otel_span_id(&current_span).or_else(|| ctx.agent_ctx.parent_span_id.clone())
        } else {
            None
        };

        // LLM integrations get a dedicated child span with the `gen_ai.*`
        // attributes so LLM Observability lands on the content-bearing model
        // call while the dispatch trace stays continuous. Integrations opt in
        // via `llm = true` in the IOA spec.
        let llm_root_span = if integration.llm {
            Some(build_llm_root_span(
                ctx,
                integration,
                entity_state,
                &module_name,
            ))
        } else {
            None
        };
        let active_span = llm_root_span.as_ref().unwrap_or(&current_span);
        let active_parent_span: Span = active_span.clone();
        active_span.record("otel.name", format!("wasm:{module_name}").as_str());
        active_span.record("wasm.module", module_name.as_str());

        let module_hash = {
            let wasm_reg = self.wasm_module_registry.read().unwrap(); // ci-ok: infallible lock
            wasm_reg
                .get_hash(ctx.entity_ref.tenant, &module_name)
                .map(|s| s.to_string())
        };

        let Some(hash) = module_hash else {
            let error_str = format!("WASM module '{}' not found", module_name);
            record_wasm_error_on_span(active_span, &error_str);
            return self
                .handle_module_not_found(ctx, integration, &module_name)
                .await;
        };
        instrument_wasm_dispatch_phase_result(
            active_parent_span.clone(),
            ctx,
            &module_name,
            WASM_DISPATCH_PHASE_MODULE_CACHE,
            self.ensure_wasm_module_cached(ctx.entity_ref.tenant, &module_name, &hash),
        )
        .await?;
        let trigger_params = instrument_wasm_dispatch_phase(
            active_parent_span.clone(),
            ctx,
            &module_name,
            WASM_DISPATCH_PHASE_REPLAY_INPUT_INJECTION,
            self.maybe_inject_ots_trajectory_actions(&module_name, ctx, action_params),
        )
        .await;

        // --- Build invocation context + host chain ---
        let (authz_ctx, mut inv_ctx) = with_wasm_dispatch_phase(
            &active_parent_span,
            ctx,
            &module_name,
            WASM_DISPATCH_PHASE_INVOCATION_CONTEXT_BUILD,
            || {
                let authz_ctx = WasmAuthzContext {
                    tenant: ctx.entity_ref.tenant.to_string(),
                    module_name: module_name.clone(),
                    agent_id: ctx.agent_ctx.agent_id.clone(),
                    session_id: ctx.agent_ctx.session_id.clone(),
                    entity_type: ctx.entity_ref.entity_type.to_string(),
                    trigger_action: ctx.action.to_string(),
                };
                let inv_ctx = WasmInvocationContext {
                    tenant: ctx.entity_ref.tenant.to_string(),
                    entity_type: ctx.entity_ref.entity_type.to_string(),
                    entity_id: ctx.entity_ref.entity_id.to_string(),
                    trigger_action: ctx.action.to_string(),
                    wasm_module: Some(module_name.clone()),
                    trigger_params,
                    entity_state: serde_json::to_value(entity_state).unwrap_or_default(),
                    agent_id: ctx.agent_ctx.agent_id.clone(),
                    session_id: ctx.agent_ctx.session_id.clone(),
                    integration_config: match self.secrets_vault.as_ref() {
                        Some(vault) => resolve_secret_templates(
                            &integration.config,
                            vault,
                            &ctx.entity_ref.tenant.to_string(),
                        ),
                        None => integration.config.clone(),
                    },
                    trace_id: current_otel_trace_id(active_span)
                        .or_else(|| ctx.agent_ctx.trace_id.clone())
                        .unwrap_or_default(),
                    workflow_root_entity_type: ctx.agent_ctx.workflow_root_entity_type.clone(),
                    workflow_root_entity_id: ctx.agent_ctx.workflow_root_entity_id.clone(),
                    workflow_run_id: ctx.agent_ctx.workflow_run_id.clone(),
                    http_request: None,
                };
                (authz_ctx, inv_ctx)
            },
        );
        if !inv_ctx.integration_config.contains_key("temper_api_url")
            && let Some(api_url) = internal_api_base_url(self)
        {
            inv_ctx
                .integration_config
                .insert("temper_api_url".to_string(), api_url);
        }
        // ADR-0046: inline-hydrate blob refs below the 128KB ceiling; defer
        // oversize refs into a blob_cache the WASM guest can read via
        // host_read_field_stream. No-op on tenants without a Turso store.
        let blob_cache = instrument_wasm_dispatch_phase(
            active_parent_span.clone(),
            ctx,
            &module_name,
            WASM_DISPATCH_PHASE_BLOB_REF_HYDRATION,
            crate::blobs::hydrate_blob_refs_for_tenant_with_ceiling(
                self,
                ctx.entity_ref.tenant,
                &mut inv_ctx.entity_state,
                crate::entity_actor::effects::DEFAULT_FIELD_INLINE_MAX,
            ),
        )
        .await;
        let denial_tracker = HttpCallAuthzDenialTracker::default();
        let gate: Arc<dyn WasmAuthzGate> = Arc::new(TrackingWasmAuthzGate::new(
            base_gate.clone(),
            denial_tracker.clone(),
        ));
        let tenant_secrets = with_wasm_dispatch_phase(
            &active_parent_span,
            ctx,
            &module_name,
            WASM_DISPATCH_PHASE_AUTHZ_SECRET_RESOLUTION,
            || {
                self.get_authorized_wasm_host_bootstrap_secrets(
                    ctx.entity_ref.tenant,
                    &*gate,
                    &authz_ctx,
                )
            },
        );
        let secret_resolver = self.authorized_wasm_secret_resolver(
            ctx.entity_ref.tenant,
            Arc::clone(&gate),
            authz_ctx.clone(),
        );
        let (host, limits) = with_wasm_dispatch_phase(
            &active_parent_span,
            ctx,
            &module_name,
            WASM_DISPATCH_PHASE_HOST_CHAIN_BUILD,
            || {
                let local_blob_interceptor = local_blob_binary_interceptor(
                    self.clone(),
                    ctx.entity_ref.tenant.clone(),
                    tenant_secrets.get("blob_endpoint").cloned(),
                );
                let local_file_interceptor = local_file_value_text_interceptor(
                    self.clone(),
                    ctx.entity_ref.tenant.clone(),
                    ctx.agent_ctx.clone(),
                    tenant_secrets.get("temper_api_url").cloned(),
                );
                // Use integration config timeout for both WASM execution and HTTP client.
                //
                // When no explicit `timeout_secs` is configured, fall back to the
                // platform default (`WasmResourceLimits::default().max_duration`, 120s
                // per ADR-0045). The fallback is observable:
                //   - `tracing::warn!` for human debugging
                //   - counter `temper_wasm_integration_default_timeout_used_total` for alerting
                //   - span attribute `wasm.timeout_source = default` for APM correlation
                //
                // Apps that fire the counter frequently should wire an explicit
                // `timeout_secs` in their integration config.
                let explicit_timeout = integration
                    .config
                    .get("timeout_secs")
                    .and_then(|s| s.parse::<u64>().ok())
                    .map(std::time::Duration::from_secs);
                let http_timeout =
                    explicit_timeout.unwrap_or_else(|| WasmResourceLimits::default().max_duration);
                if explicit_timeout.is_some() {
                    active_span.record("wasm.timeout_source", "explicit");
                } else {
                    active_span.record("wasm.timeout_source", "default");
                    // ADR-0054 warn-audit: default-timeout fallback is a configuration
                    // observation, not an actionable condition. The metric below is
                    // the alerting signal; this log is purely local-dev diagnostic.
                    tracing::debug!(
                        tenant = %ctx.entity_ref.tenant,
                        entity_type = ctx.entity_ref.entity_type,
                        entity_id = ctx.entity_ref.entity_id,
                        integration = %integration.name,
                        module = %module_name,
                        default_timeout_secs = http_timeout.as_secs(),
                        "WASM integration falling back to default timeout — wire `timeout_secs` explicitly in integration config"
                    );
                    crate::runtime_metrics::record_wasm_default_timeout_used(
                        ctx.entity_ref.tenant.as_str(),
                        ctx.entity_ref.entity_type,
                        module_name.as_str(),
                    );
                }
                let progress_emitter = progress_emitter_fn(
                    self.clone(),
                    ctx.entity_ref.tenant.to_string(),
                    ctx.entity_ref.entity_type.to_string(),
                    ctx.entity_ref.entity_id.to_string(),
                    module_name.clone(),
                );
                let host_invocation_context = inv_ctx.clone();
                let internal_api_key = std::env::var("TEMPER_API_KEY").ok(); // determinism-ok: production host loopback config
                let internal_api_url = internal_api_base_url(self);
                let mut production_host_builder =
                    ProductionWasmHost::with_timeout(tenant_secrets, http_timeout)
                        .with_binary_http_interceptor(
                            local_blob_interceptor
                                .unwrap_or_else(|| Arc::new(|_, _, _, _| Box::pin(async { None }))),
                        )
                        .with_spec_evaluator(spec_evaluator_fn())
                        .with_progress_emitter(progress_emitter)
                        .with_internal_api_base_url(internal_api_url)
                        .with_internal_api_key(internal_api_key)
                        .with_invocation_context(host_invocation_context)
                        .with_text_http_interceptor(
                            local_file_interceptor
                                .unwrap_or_else(|| Arc::new(|_, _, _, _| Box::pin(async { None }))),
                        )
                        .with_trace_id(
                            current_otel_trace_id(active_span)
                                .or_else(|| ctx.agent_ctx.trace_id.clone()),
                        );
                if let Some(resolver) = secret_resolver.clone() {
                    production_host_builder =
                        production_host_builder.with_secret_resolver(resolver);
                }
                let production_host: Arc<dyn WasmHost> = Arc::new(production_host_builder);
                let inner: Arc<dyn WasmHost> = Arc::new(LocalTDataWasmHost::new(
                    self.clone(),
                    ctx.entity_ref.tenant.clone(),
                    ctx.agent_ctx.security_ctx.as_ref(),
                    production_host,
                ));
                let host: Arc<dyn WasmHost> =
                    Arc::new(AuthorizedWasmHost::new(inner, gate, authz_ctx));
                let max_response_bytes = integration
                    .config
                    .get("max_response_bytes")
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(WasmResourceLimits::default().max_response_bytes);
                let max_fuel = integration
                    .config
                    .get("max_fuel")
                    .and_then(|s| s.parse::<u64>().ok())
                    .unwrap_or(WasmResourceLimits::default().max_fuel);
                let max_memory = integration
                    .config
                    .get("max_memory")
                    .and_then(|s| s.parse::<usize>().ok())
                    .unwrap_or(WasmResourceLimits::default().max_memory);
                let limits = WasmResourceLimits {
                    max_duration: http_timeout,
                    max_response_bytes,
                    max_fuel,
                    max_memory,
                };
                (host, limits)
            },
        );

        with_wasm_dispatch_phase(
            &active_parent_span,
            ctx,
            &module_name,
            WASM_DISPATCH_PHASE_INTEGRATION_OBSERVE_START,
            || {
                tracing::info!(
                    tenant = %ctx.entity_ref.tenant,
                    entity_type = ctx.entity_ref.entity_type,
                    entity_id = ctx.entity_ref.entity_id,
                    integration = %integration.name,
                    module = %module_name,
                    hash = %hash,
                    "invoking WASM integration module"
                );
                let start_seq = self.next_entity_event_sequence(
                    ctx.entity_ref.tenant.as_str(),
                    ctx.entity_ref.entity_type,
                    ctx.entity_ref.entity_id,
                );
                self.record_entity_observe_event_with_seq(
                    ctx.entity_ref.tenant.as_str(),
                    ctx.entity_ref.entity_type,
                    ctx.entity_ref.entity_id,
                    start_seq,
                    "integration_start",
                    serde_json::json!({
                        "seq": start_seq,
                        "integration": integration.name,
                        "module": module_name,
                        "trigger_action": ctx.action,
                    }),
                );
            },
        );

        // --- Invoke and handle result ---
        let invoke = instrument_wasm_dispatch_phase_result(
            active_parent_span,
            ctx,
            &module_name,
            WASM_DISPATCH_PHASE_ENGINE_INVOKE_AND_HANDLE,
            self.invoke_and_handle_result(
                ctx,
                integration,
                &module_name,
                &hash,
                entity_state,
                inv_ctx,
                host,
                &limits,
                &denial_tracker,
                blob_cache,
                llm_parent_span_id.as_deref(),
            ),
        );

        if let Some(span) = llm_root_span {
            invoke.instrument(span).await
        } else {
            invoke.await
        }
    }
}
