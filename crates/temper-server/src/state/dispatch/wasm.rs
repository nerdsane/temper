use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use serde_json::Value;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{Span, instrument};

use crate::entity_actor::{EntityResponse, EntityState};
use crate::request_context::AgentContext;
use crate::secrets::template::resolve_secret_templates;
use crate::state::sim_now;
use temper_runtime::tenant::TenantId;
use temper_wasm::{
    AuthorizedWasmHost, BinaryHttpInterceptorFn, ProductionWasmHost, ProgressEmitterFn,
    StreamRegistry, WasmAuthzContext, WasmAuthzGate, WasmHost, WasmInvocationContext,
    WasmResourceLimits,
};

use super::{
    HttpCallAuthzDenialTracker, TrackingWasmAuthzGate, WasmDispatchMode, WasmDispatchRequest,
    WasmEntityRef,
};
use replay_inputs::{extract_trajectory_actions_from_ots, has_replay_trajectory_input};

mod invocation_artifacts;
mod replay_inputs;

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

fn monty_repl_max_concurrency() -> usize {
    static MAX_CONCURRENCY: OnceLock<usize> = OnceLock::new();
    *MAX_CONCURRENCY.get_or_init(|| {
        std::env::var("TEMPER_MONTY_REPL_MAX_CONCURRENCY") // determinism-ok: read once at startup
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(2)
    })
}

fn monty_repl_semaphore() -> &'static Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    SEMAPHORE.get_or_init(|| Arc::new(Semaphore::new(monty_repl_max_concurrency())))
}

fn monty_repl_active_counter() -> &'static AtomicU64 {
    static ACTIVE: AtomicU64 = AtomicU64::new(0);
    &ACTIVE
}

struct MontyReplExecutionPermit {
    _permit: OwnedSemaphorePermit,
    max_concurrency: usize,
}

impl MontyReplExecutionPermit {
    async fn acquire() -> Self {
        let max_concurrency = monty_repl_max_concurrency();
        let wait_started = Instant::now();
        let permit = monty_repl_semaphore()
            .clone()
            .acquire_owned()
            .await
            .expect("monty_repl semaphore should not be closed");
        let wait_duration = wait_started.elapsed();
        crate::runtime_metrics::record_monty_repl_acquisition(max_concurrency);
        crate::runtime_metrics::record_monty_repl_wait_duration(wait_duration, max_concurrency);
        let active = monty_repl_active_counter().fetch_add(1, Ordering::SeqCst) + 1;
        crate::runtime_metrics::record_monty_repl_active_invocations(active, max_concurrency);
        tracing::info!(
            module = MONTY_REPL_MODULE,
            wait_ms = wait_duration.as_millis() as u64,
            active_invocations = active,
            max_concurrency,
            "acquired monty_repl execution permit"
        );
        Self {
            _permit: permit,
            max_concurrency,
        }
    }
}

impl Drop for MontyReplExecutionPermit {
    fn drop(&mut self) {
        let previous = monty_repl_active_counter().fetch_sub(1, Ordering::SeqCst);
        let active = previous.saturating_sub(1);
        crate::runtime_metrics::record_monty_repl_active_invocations(active, self.max_concurrency);
        tracing::info!(
            module = MONTY_REPL_MODULE,
            active_invocations = active,
            max_concurrency = self.max_concurrency,
            "released monty_repl execution permit"
        );
    }
}

fn http_call_authz_denied_error(reason: &str) -> String {
    format!("{HTTP_CALL_AUTHZ_DENIED_PREFIX}: {reason}")
}

fn is_http_call_authz_denial(error: &str) -> bool {
    error.contains(HTTP_CALL_AUTHZ_DENIED_PREFIX)
}

fn is_local_internal_blob_endpoint(endpoint: &str) -> bool {
    let normalized = endpoint.trim_end_matches('/');
    (normalized.starts_with("http://127.0.0.1:") || normalized.starts_with("http://localhost:"))
        && normalized.contains("/_internal/blobs")
}

fn local_blob_binary_interceptor(
    store: Option<temper_store_turso::TursoEventStore>,
    blob_endpoint: Option<String>,
) -> Option<BinaryHttpInterceptorFn> {
    let store = store?;
    let endpoint = blob_endpoint?;
    if !is_local_internal_blob_endpoint(&endpoint) {
        return None;
    }

    let endpoint = endpoint.trim_end_matches('/').to_string();
    Some(Arc::new(move |method, url, _headers, body| {
        let store = store.clone();
        let endpoint = endpoint.clone();
        Box::pin(async move {
            let prefix = format!("{endpoint}/");
            let blob_key = url.strip_prefix(&prefix)?;
            let blob_key = blob_key.to_string();
            crate::runtime_metrics::record_blob_local_fast_path_request(&method);
            tracing::info!(
                method = %method,
                blob_key = %blob_key,
                "handling local blob request without loopback HTTP"
            );

            let result = match method.as_str() {
                "PUT" => crate::blobs::put_blob_bytes(&store, &blob_key, &body)
                    .await
                    .map(|()| (204, Vec::new())),
                "GET" => crate::blobs::get_blob_bytes(&store, &blob_key)
                    .await
                    .map(|maybe| match maybe {
                        Some(bytes) => (200, bytes),
                        None => (404, Vec::new()),
                    }),
                other => Err(format!("unsupported local blob method: {other}")),
            };

            Some(result)
        })
    }))
}

impl crate::state::ServerState {
    #[instrument(skip_all, fields(otel.name = %format_args!("{}.{}.integrations", req.entity_type, req.action), tenant = %req.tenant, entity_type = req.entity_type, entity_id = req.entity_id, action_name = req.action))]
    pub(crate) async fn dispatch_wasm_integrations_internal(
        &self,
        req: &WasmDispatchRequest<'_>,
    ) -> Result<Option<EntityResponse>, String> {
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
        gen_ai.system = tracing::field::Empty,
        gen_ai.request.model = tracing::field::Empty,
        gen_ai.operation.name = tracing::field::Empty,
        gen_ai.response.finish_reasons = tracing::field::Empty,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.conversation.id = tracing::field::Empty,
        gen_ai.input.messages = tracing::field::Empty,
        gen_ai.output.messages = tracing::field::Empty,
        error.type = tracing::field::Empty,
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

        // Set dynamic span name and GenAI attributes
        let span = tracing::Span::current();
        span.record("otel.name", format!("wasm:{module_name}").as_str());
        span.record("wasm.module", module_name.as_str());
        if module_name == "llm_caller" {
            // Resolve provider dynamically from entity state (default: anthropic)
            let provider = entity_state
                .fields
                .get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("anthropic");
            span.record("gen_ai.system", provider);
            span.record("gen_ai.operation.name", "chat");
            if let Some(model) = entity_state.fields.get("model").and_then(|v| v.as_str()) {
                span.record("gen_ai.request.model", model);
            }
            // Session ID for grouping turns in Datadog LLM Observability
            if let Some(ref session_id) = ctx.agent_ctx.session_id {
                span.record("gen_ai.conversation.id", session_id.as_str());
            } else {
                // Fall back to entity_id as session grouping key
                span.record("gen_ai.conversation.id", ctx.entity_ref.entity_id);
            }
        }

        let module_hash = {
            let wasm_reg = self.wasm_module_registry.read().unwrap(); // ci-ok: infallible lock
            wasm_reg
                .get_hash(ctx.entity_ref.tenant, &module_name)
                .map(|s| s.to_string())
        };

        let Some(hash) = module_hash else {
            return self
                .handle_module_not_found(ctx, integration, &module_name)
                .await;
        };
        let trigger_params = self
            .maybe_inject_ots_trajectory_actions(&module_name, ctx, action_params)
            .await;

        // --- Build invocation context + host chain ---
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
            trace_id: ctx.agent_ctx.trace_id.clone().unwrap_or_default(),
        };
        let denial_tracker = HttpCallAuthzDenialTracker::default();
        let gate: Arc<dyn WasmAuthzGate> = Arc::new(TrackingWasmAuthzGate::new(
            base_gate.clone(),
            denial_tracker.clone(),
        ));
        let tenant_secrets =
            self.get_authorized_wasm_secrets(ctx.entity_ref.tenant, &*gate, &authz_ctx);
        let local_blob_interceptor = local_blob_binary_interceptor(
            self.platform_persistent_store().cloned(),
            tenant_secrets.get("blob_endpoint").cloned(),
        );
        // Use integration config timeout for both WASM execution and HTTP client.
        let http_timeout = integration
            .config
            .get("timeout_secs")
            .and_then(|s| s.parse::<u64>().ok())
            .map(std::time::Duration::from_secs)
            .unwrap_or(std::time::Duration::from_secs(30));
        let progress_emitter = progress_emitter_fn(
            self.clone(),
            ctx.entity_ref.tenant.to_string(),
            ctx.entity_ref.entity_type.to_string(),
            ctx.entity_ref.entity_id.to_string(),
            module_name.clone(),
        );
        let host_invocation_context = inv_ctx.clone();
        let inner: Arc<dyn WasmHost> = Arc::new(
            ProductionWasmHost::with_timeout(tenant_secrets, http_timeout)
                .with_binary_http_interceptor(
                    local_blob_interceptor
                        .unwrap_or_else(|| Arc::new(|_, _, _, _| Box::pin(async { None }))),
                )
                .with_spec_evaluator(spec_evaluator_fn())
                .with_progress_emitter(progress_emitter)
                .with_invocation_context(host_invocation_context)
                .with_trace_id(ctx.agent_ctx.trace_id.clone()),
        );
        let host: Arc<dyn WasmHost> = Arc::new(AuthorizedWasmHost::new(inner, gate, authz_ctx));
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

        // --- Invoke and handle result ---
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
        )
        .await
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
        let turso = self.persistent_store_for_tenant(tenant).await?;
        let agent_id = ctx.agent_ctx.agent_id.as_deref();

        let mut rows = turso
            .list_ots_trajectories(tenant, agent_id, None, 50)
            .await
            .ok()?;

        // Fallback when identity resolution was unavailable at upload time.
        if rows.is_empty() && agent_id.is_some() {
            rows = turso
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
            let data = match turso
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
    ) -> Result<Option<EntityResponse>, String> {
        // Existing action-triggered invocations don't use streams — pass empty registry.
        let streams = Arc::new(std::sync::RwLock::new(StreamRegistry::default()));
        let _monty_repl_permit = if module_name == MONTY_REPL_MODULE {
            Some(MontyReplExecutionPermit::acquire().await)
        } else {
            None
        };
        match self
            .wasm_engine
            .invoke(hash, &inv_ctx, host, limits, streams)
            .await
        {
            Ok(result) if result.success => {
                let callback_params = &result.callback_params;

                // Record GenAI token usage from callback params (if present)
                if let Some(input) = callback_params.get("input_tokens").and_then(|v| v.as_i64())
                {
                    Span::current().record("gen_ai.usage.input_tokens", input);
                }
                if let Some(output) = callback_params.get("output_tokens").and_then(|v| v.as_i64())
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
                // Dynamic provider override (if WASM module reports actual provider used)
                if let Some(provider) = callback_params.get("_gen_ai_provider").and_then(|v| v.as_str())
                {
                    Span::current().record("gen_ai.system", provider);
                }
                if let Some(finish_reason) = callback_params
                    .get("_gen_ai_finish_reason")
                    .and_then(|v| v.as_str())
                    .filter(|value| !value.is_empty())
                {
                    Span::current().record("gen_ai.response.finish_reasons", finish_reason);
                }

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
                if let Some(reason) = denial_tracker.take_denial() {
                    let error_str = http_call_authz_denied_error(&reason);
                    if module_name == "llm_caller" {
                        Span::current()
                            .record("error.type", llm_error_type(&error_str).as_str());
                    }
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

                if module_name == "llm_caller" {
                    let event = llm_call_wide_event(
                        ctx,
                        entity_state,
                        callback_params,
                        result.duration_ms,
                    );
                    temper_observe::wide_event::emit_span(&event);
                    temper_observe::wide_event::emit_metrics(&event);
                }

                self.record_invocation(
                    ctx.entity_ref,
                    module_name,
                    ctx.action,
                    Some(result.callback_action.clone()),
                    true,
                    None,
                    result.duration_ms,
                    None,
                )
                .await;

                // Determine callback action: prefer static on_success from spec,
                // fall back to dynamic callback_action from WASM result.
                let callback_action = integration
                    .on_success
                    .as_deref()
                    .unwrap_or(&result.callback_action);

                if !callback_action.is_empty()
                    && let Some(resp) = self
                        .dispatch_wasm_callback(
                            ctx.entity_ref,
                            callback_action,
                            result.callback_params,
                            ctx.agent_ctx,
                            ctx.mode,
                        )
                        .await?
                {
                    return Ok(Some(resp));
                }
                Ok(None)
            }
            Ok(result) => {
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
                let mut error_str = result.error.unwrap_or_else(|| {
                    format!(
                        "WASM integration '{}' returned unsuccessful result",
                        integration.name
                    )
                });
                if let Some(reason) = denial_tracker.take_denial() {
                    error_str = http_call_authz_denied_error(&reason);
                }
                if module_name == "llm_caller" {
                    Span::current().record("error.type", llm_error_type(&error_str).as_str());
                }
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
                let mut error_str = e.to_string();
                if let Some(reason) = denial_tracker.take_denial()
                    && !is_http_call_authz_denial(&error_str)
                {
                    error_str = http_call_authz_denied_error(&reason);
                }
                if module_name == "llm_caller" {
                    Span::current().record("error.type", llm_error_type(&error_str).as_str());
                }
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
        context: WasmInvocationContext,
        streams: Arc<std::sync::RwLock<StreamRegistry>>,
    ) -> Result<temper_wasm::WasmInvocationResult, String> {
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
        let tenant_secrets = self.get_authorized_wasm_secrets(tenant, &*base_gate, &authz_ctx);
        let local_blob_interceptor = local_blob_binary_interceptor(
            self.platform_persistent_store().cloned(),
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
            .with_invocation_context(context.clone());
        if let Some(interceptor) = local_blob_interceptor {
            base_host = base_host.with_binary_http_interceptor(interceptor);
        }
        let inner: Arc<dyn WasmHost> = Arc::new(base_host);
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

fn llm_call_wide_event<'a>(
    ctx: &'a WasmDispatchCtx<'a>,
    entity_state: &'a EntityState,
    callback_params: &'a Value,
    duration_ms: u64,
) -> temper_observe::wide_event::WideEvent {
    let provider = callback_params
        .get("_gen_ai_provider")
        .and_then(Value::as_str)
        .or_else(|| entity_state.fields.get("provider").and_then(Value::as_str))
        .unwrap_or("unknown");
    let model = entity_state
        .fields
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let session_id = ctx
        .agent_ctx
        .session_id
        .as_deref()
        .unwrap_or(ctx.entity_ref.entity_id);
    let input_tokens = callback_params
        .get("input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let output_tokens = callback_params
        .get("output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let stop_reason = callback_params
        .get("_gen_ai_finish_reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let input_messages = callback_params
        .get("_gen_ai_input_messages")
        .and_then(Value::as_str);
    let output_messages = callback_params
        .get("_gen_ai_output_messages")
        .and_then(Value::as_str);
    let trace_id = ctx.agent_ctx.trace_id.as_deref().unwrap_or_default();

    temper_observe::wide_event::from_llm_call(temper_observe::wide_event::LlmCallInput {
        provider,
        model,
        operation: "chat",
        entity_type: ctx.entity_ref.entity_type,
        entity_id: ctx.entity_ref.entity_id,
        session_id,
        success: true,
        duration_ns: duration_ms * 1_000_000,
        input_tokens,
        output_tokens,
        stop_reason,
        input_messages,
        output_messages,
        trace_id,
        error: None,
    })
}

fn llm_error_type(error: &str) -> String {
    let normalized = error.to_ascii_lowercase();
    if normalized.contains("rate limit") {
        "rate_limit".to_string()
    } else if normalized.contains("timeout") {
        "timeout".to_string()
    } else if normalized.contains("authorization denied") {
        "authorization_denied".to_string()
    } else if normalized.contains("connection") {
        "connection_error".to_string()
    } else {
        "integration_error".to_string()
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
