use std::sync::Arc;

use serde_json::Value;
use tracing::{Instrument, Span, instrument};

use crate::entity_actor::{EntityResponse, EntityState};
use crate::request_context::AgentContext;
use crate::secrets::template::resolve_secret_templates;
use crate::state::sim_now;
use temper_runtime::tenant::TenantId;
use temper_wasm::{
    AuthorizedWasmHost, ProductionWasmHost, ProgressEmitterFn, StreamRegistry, WasmAuthzContext,
    WasmAuthzGate, WasmHost, WasmInvocationContext, WasmResourceLimits,
};

use super::{
    HttpCallAuthzDenialTracker, TrackingWasmAuthzGate, WasmDispatchMode, WasmDispatchRequest,
    WasmEntityRef, record_workflow_span_attrs,
};
use replay_inputs::{extract_trajectory_actions_from_ots, has_replay_trajectory_input};

mod interceptors;
mod invocation_artifacts;
mod llm_observability;
mod local_tdata_host;
mod phases;
mod replay_inputs;

use interceptors::{
    internal_api_base_url, local_blob_binary_interceptor, local_file_value_text_interceptor,
};
pub(super) use llm_observability::record_wasm_error_on_current_span;
use llm_observability::{
    attach_llm_parent_context, build_llm_root_span, current_otel_span_id, current_otel_trace_id,
    llm_call_wide_event, llm_model_for_observability, llm_provider_for_observability,
    record_wasm_error_on_span, should_record_gen_ai_span_attrs, strip_private_observability_params,
    submit_llmobs_llm_span, submit_llmobs_tool_spans,
};
use local_tdata_host::LocalTDataWasmHost;
use phases::{
    WASM_DISPATCH_PHASE_AUTHZ_SECRET_RESOLUTION, WASM_DISPATCH_PHASE_BLOB_REF_HYDRATION,
    WASM_DISPATCH_PHASE_DISPATCH_CALLBACK, WASM_DISPATCH_PHASE_ENGINE_INVOKE,
    WASM_DISPATCH_PHASE_ENGINE_INVOKE_AND_HANDLE, WASM_DISPATCH_PHASE_HOST_CHAIN_BUILD,
    WASM_DISPATCH_PHASE_INTEGRATION_OBSERVE_START, WASM_DISPATCH_PHASE_INVOCATION_CONTEXT_BUILD,
    WASM_DISPATCH_PHASE_LLMOBS_SUBMIT, WASM_DISPATCH_PHASE_MODULE_CACHE,
    WASM_DISPATCH_PHASE_RECORD_INVOCATION, WASM_DISPATCH_PHASE_REPLAY_INPUT_INJECTION,
    WASM_DISPATCH_PHASE_RESULT_OBSERVE_COMPLETE, instrument_wasm_dispatch_phase,
    instrument_wasm_dispatch_phase_result, with_wasm_dispatch_phase,
};

/// Shared context threaded through the WASM dispatch call chain.
///
/// Bundles the entity reference, trigger action, agent identity, and dispatch
/// mode so individual functions don't need to accept them as separate params.
struct WasmDispatchCtx<'a> {
    entity_ref: WasmEntityRef<'a>,
    action: &'a str,
    agent_ctx: &'a AgentContext,
    mode: WasmDispatchMode,
}

const HTTP_CALL_AUTHZ_DENIED_PREFIX: &str = "authorization denied for http_call";
const MONTY_REPL_MODULE: &str = "monty_repl";

fn http_call_authz_denied_error(reason: &str) -> String {
    format!("{HTTP_CALL_AUTHZ_DENIED_PREFIX}: {reason}")
}

fn is_http_call_authz_denial(error: &str) -> bool {
    error.contains(HTTP_CALL_AUTHZ_DENIED_PREFIX)
}

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

            if let Some(resp) = self
                .dispatch_single_integration(
                    &ctx,
                    &integration,
                    req.entity_state,
                    req.action_params,
                    &base_gate,
                )
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
    async fn dispatch_single_integration(
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

    /// Fill missing replay trajectory inputs from persisted OTS traces.
    async fn maybe_inject_ots_trajectory_actions(
        &self,
        module_name: &str,
        ctx: &WasmDispatchCtx<'_>,
        action_params: &Value,
    ) -> Value {
        if module_name != "gepa-replay" || has_replay_trajectory_input(action_params) {
            return action_params.clone();
        }

        let Some((trajectories, actions)) = self.load_replay_inputs_from_ots(ctx).await else {
            tracing::warn!(
                tenant = %ctx.entity_ref.tenant,
                entity_type = ctx.entity_ref.entity_type,
                entity_id = ctx.entity_ref.entity_id,
                trigger = ctx.action,
                "gepa-replay missing Trajectories/TrajectoryActions and no usable OTS trajectories found"
            );
            return action_params.clone();
        };

        tracing::info!(
            tenant = %ctx.entity_ref.tenant,
            entity_type = ctx.entity_ref.entity_type,
            entity_id = ctx.entity_ref.entity_id,
            trigger = ctx.action,
            trajectory_count = trajectories.len(),
            action_count = actions.len(),
            "gepa-replay Trajectories and TrajectoryActions auto-injected from OTS"
        );

        let mut params = action_params.clone();
        if let Some(obj) = params.as_object_mut() {
            obj.insert(
                "Trajectories".to_string(),
                Value::Array(trajectories.clone()),
            );
            obj.insert(
                "TrajectoryActions".to_string(),
                Value::Array(actions.clone()),
            );
            obj.insert("TrajectorySource".to_string(), serde_json::json!("ots"));
            obj.insert(
                "TrajectoryCount".to_string(),
                serde_json::json!(trajectories.len()),
            );
            obj.insert(
                "TrajectoryActionsCount".to_string(),
                serde_json::json!(actions.len()),
            );
            return params;
        }

        serde_json::json!({
            "Trajectories": trajectories,
            "TrajectoryActions": actions,
            "TrajectorySource": "ots",
            "OriginalTriggerParams": action_params,
        })
    }

    async fn load_replay_inputs_from_ots(
        &self,
        ctx: &WasmDispatchCtx<'_>,
    ) -> Option<(Vec<Value>, Vec<Value>)> {
        let tenant = ctx.entity_ref.tenant.as_str();
        let store = self.metadata_store_for_tenant(tenant).await?;
        let agent_id = ctx.agent_ctx.agent_id.as_deref();

        let mut rows = store
            .list_ots_trajectories(tenant, agent_id, None, 50)
            .await
            .ok()?;

        // Fallback when identity resolution was unavailable at upload time.
        if rows.is_empty() && agent_id.is_some() {
            rows = store
                .list_ots_trajectories(tenant, None, None, 50)
                .await
                .ok()?;
        }

        let session_id = ctx.agent_ctx.session_id.as_deref();
        if let Some(session) = session_id {
            rows.sort_by_key(|row| if row.session_id == session { 0 } else { 1 });
        }

        let mut trajectories = Vec::new();
        let mut actions = Vec::new();

        for row in rows {
            let data = match store
                .get_ots_trajectory(&row.trajectory_id)
                .await
                .ok()
                .flatten()
            {
                Some(d) => d,
                None => continue,
            };
            let trajectory = match serde_json::from_str::<Value>(&data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            let extracted = extract_trajectory_actions_from_ots(&trajectory);
            let has_turns = trajectory
                .get("turns")
                .and_then(Value::as_array)
                .map(|turns| !turns.is_empty())
                .unwrap_or(false);

            if has_turns || !extracted.is_empty() {
                trajectories.push(trajectory);
                actions.extend(extracted);
            }
        }

        if trajectories.is_empty() && actions.is_empty() {
            None
        } else {
            Some((trajectories, actions))
        }
    }

    /// Handle module-not-found: log, observe, dispatch on_failure callback.
    async fn handle_module_not_found(
        &self,
        ctx: &WasmDispatchCtx<'_>,
        integration: &temper_spec::automaton::Integration,
        module_name: &str,
    ) -> Result<Option<EntityResponse>, String> {
        tracing::warn!(
            tenant = %ctx.entity_ref.tenant,
            entity_type = ctx.entity_ref.entity_type,
            module = %module_name,
            "WASM module not found in registry"
        );
        let error_str = format!("WASM module '{}' not found", module_name);
        self.record_invocation(
            ctx.entity_ref,
            module_name,
            ctx.action,
            integration.on_failure.clone(),
            false,
            Some(error_str.clone()),
            0,
            None,
        )
        .await;

        if let Some(ref cb) = integration.on_failure {
            let params = serde_json::json!({
                "error": error_str,
                "integration": integration.name.clone(),
            });
            return self
                .dispatch_wasm_callback(ctx.entity_ref, cb, params, ctx.agent_ctx, ctx.mode)
                .await;
        }
        Ok(None)
    }

    /// Invoke the WASM module and handle success/failure/error results.
    #[allow(clippy::too_many_arguments)]
    async fn invoke_and_handle_result(
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

                // Determine callback action: prefer static on_success from spec,
                // fall back to dynamic callback_action from WASM result.
                let callback_action = integration
                    .on_success
                    .as_deref()
                    .unwrap_or(&result.callback_action);

                if !callback_action.is_empty() {
                    let callback_response = instrument_wasm_dispatch_phase_result(
                        phase_parent_span.clone(),
                        ctx,
                        module_name,
                        WASM_DISPATCH_PHASE_DISPATCH_CALLBACK,
                        self.dispatch_wasm_callback(
                            ctx.entity_ref,
                            callback_action,
                            strip_private_observability_params(result.callback_params),
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

/// Build a spec evaluator closure that uses `temper-jit` to evaluate transitions.
///
/// This bridges `temper-wasm` (no jit dep) and `temper-jit` (transition evaluation)
/// through a function pointer injected into `ProductionWasmHost`.
fn spec_evaluator_fn() -> temper_wasm::SpecEvaluatorFn {
    use temper_jit::table::TransitionTable;
    use temper_spec::automaton::parse_automaton;

    std::sync::Arc::new(
        |ioa_source: &str, current_state: &str, action: &str, _params_json: &str| {
            let automaton = parse_automaton(ioa_source)
                .map_err(|e| format!("failed to parse IOA spec: {e}"))?;
            let table = TransitionTable::from_automaton(&automaton);

            // evaluate(current_state, item_count, action) -> Option<TransitionResult>
            match table.evaluate(current_state, 0, action) {
                Some(result) => {
                    let json = serde_json::json!({
                        "success": result.success,
                        "new_state": result.new_state,
                        "error": serde_json::Value::Null,
                    });
                    Ok(json.to_string())
                }
                None => {
                    let json = serde_json::json!({
                        "success": false,
                        "new_state": serde_json::Value::Null,
                        "error": format!("unknown action '{}' in state '{}'", action, current_state),
                    });
                    Ok(json.to_string())
                }
            }
        },
    )
}

fn progress_emitter_fn(
    state: crate::state::ServerState,
    tenant: String,
    entity_type: String,
    entity_id: String,
    module_name: String,
) -> ProgressEmitterFn {
    std::sync::Arc::new(move |event_json: &str| {
        let parsed = serde_json::from_str::<Value>(event_json).unwrap_or_else(|_| {
            serde_json::json!({
                "kind": "integration_progress",
                "message": event_json,
            })
        });
        let kind = parsed
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("integration_progress")
            .to_string();
        let seq = state.next_entity_event_sequence(&tenant, &entity_type, &entity_id);
        let event = crate::state::AgentProgressEvent {
            tenant: tenant.clone(),
            entity_type: entity_type.clone(),
            entity_id: entity_id.clone(),
            seq,
            kind,
            agent_id: entity_id.clone(),
            tool_call_id: parsed
                .get("tool_call_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            tool_name: parsed
                .get("tool_name")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| Some(module_name.clone())),
            task_id: parsed
                .get("task_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            message: parsed
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_string),
            timestamp: sim_now().to_rfc3339(),
            data: Some(parsed),
        };
        state.broadcast_agent_progress(event);
        Ok(())
    })
}
