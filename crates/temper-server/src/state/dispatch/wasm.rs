use std::future::Future;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use opentelemetry::trace::{Status, TraceContextExt};
use serde_json::{Value, json};
use tracing::{Instrument, Span, instrument};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::application_data::{ApplicationDataInvocation, ModuleInvocationAuthority};
use crate::entity_actor::{EntityResponse, EntityState};
use crate::request_context::AgentContext;
use crate::secrets::template::resolve_secret_templates;
use crate::state::sim_now;
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::tenant::TenantId;
use temper_wasm::host_trait::clamp_redacted_metadata_value;
use temper_wasm::{
    AuthorizedWasmHost, BinaryHttpInterceptorFn, InternalHttpCapability,
    InternalHttpCapabilityIssuerFn, ProductionWasmHost, ProgressEmitterFn, StreamRegistry,
    TemperDataCallFn, TemperFileReadFn, TemperFileWriteFn, TextHttpInterceptorFn, WasmAuthzContext,
    WasmAuthzGate, WasmHost, WasmInvocationContext, WasmResourceLimits,
};

use super::{
    HttpCallAuthzDenialTracker, TrackingWasmAuthzGate, WasmDispatchMode, WasmDispatchRequest,
    WasmEntityRef, record_workflow_span_attrs,
};
use replay_inputs::{extract_trajectory_actions_from_ots, has_replay_trajectory_input};

mod boxed;
mod invocation_artifacts;
mod local_tdata_host;
mod replay_inputs;

pub(super) use boxed::{
    dispatch_tenant_action_core_boxed, dispatch_wasm_callback_boxed,
    dispatch_wasm_integrations_boxed,
};
use boxed::{handle_wasm_failure_boxed, invoke_and_handle_result_boxed};
use local_tdata_host::LocalTDataWasmHost;

/// Build a request-bound internal HTTP capability issuer for a non-System caller.
pub(crate) fn internal_http_capability_issuer(
    state: &crate::state::ServerState,
    tenant: &TenantId,
    security_context: Option<&SecurityContext>,
) -> Option<InternalHttpCapabilityIssuerFn> {
    let security_context = security_context?;
    if security_context.principal.kind == PrincipalKind::System {
        return None;
    }
    let authenticated = AuthenticatedRequestContext::new(tenant.clone(), security_context.clone());
    let tenant = tenant.clone();
    let store = state.internal_invocation_credentials.clone();
    Some(Arc::new(move |method, url| {
        let bearer = store
            .issue_for_url(authenticated.clone(), method, url)
            .map_err(|error| error.to_string())?;
        InternalHttpCapability::new(bearer, tenant.to_string())
    }))
}

/// Build an internal HTTP capability issuer bound to the immutable WASM
/// module identity already admitted by the host-function Cedar gate.
pub(crate) fn internal_wasm_http_capability_issuer(
    state: &crate::state::ServerState,
    tenant: &TenantId,
    wasm: &WasmAuthzContext,
) -> InternalHttpCapabilityIssuerFn {
    let security = crate::authz::wasm_gate::build_wasm_security_context(wasm);
    match internal_http_capability_issuer(state, tenant, Some(&security)) {
        Some(issuer) => issuer,
        None => Arc::new(|_, _| {
            Err("WASM module authority cannot issue an internal HTTP capability".to_string())
        }),
    }
}

/// Build the same Cedar-gated host chain for an inbound `HttpEndpoint` guest
/// that ordinary action-triggered WASM integrations receive.
///
/// The shared HTTP stream registry is the only endpoint-specific transport
/// detail. Secret access, outbound HTTP, local TData calls, and internal HTTP
/// re-entry all use the canonical authorization components.
pub(crate) fn authorized_http_endpoint_host(
    state: &crate::state::ServerState,
    tenant: &TenantId,
    module_name: &str,
    invocation_context: &WasmInvocationContext,
    http_streams: Arc<temper_wasm::http_stream::HttpStreamRegistry>,
) -> Result<Arc<dyn WasmHost>, String> {
    let gate = state.wasm_authz_gate();
    let authz_context = WasmAuthzContext {
        tenant: tenant.to_string(),
        module_name: module_name.to_string(),
        agent_id: invocation_context.agent_id.clone(),
        session_id: invocation_context.session_id.clone(),
        entity_type: invocation_context.entity_type.clone(),
        trigger_action: invocation_context.trigger_action.clone(),
    };
    let bootstrap_secrets =
        state.get_authorized_wasm_host_bootstrap_secrets(tenant, &*gate, &authz_context);
    let gate = crate::authz::wasm_gate::bind_local_blob_endpoint(
        gate,
        bootstrap_secrets.get("blob_endpoint").map(String::as_str),
    );
    let secret_resolver =
        state.authorized_wasm_secret_resolver(tenant, Arc::clone(&gate), authz_context.clone());
    let capability_issuer = internal_wasm_http_capability_issuer(state, tenant, &authz_context);
    let internal_api_url = internal_api_base_url(state);
    let local_blob_interceptor = local_blob_binary_interceptor(
        state.clone(),
        tenant.clone(),
        bootstrap_secrets.get("blob_endpoint").cloned(),
    );
    let progress_emitter = progress_emitter_fn(
        state.clone(),
        tenant.to_string(),
        invocation_context.entity_type.clone(),
        invocation_context.entity_id.clone(),
        module_name.to_string(),
    );

    let mut base_host = ProductionWasmHost::with_shared_streams(bootstrap_secrets, http_streams)
        .with_spec_evaluator(spec_evaluator_fn())
        .with_progress_emitter(progress_emitter)
        .with_internal_api_base_url(internal_api_url)
        .with_internal_capability_issuer(capability_issuer)
        .with_invocation_context(invocation_context.clone())
        // ARN-243: the HttpEndpoint path honours the same per-tenant LLM content
        // export decision as the integration path above.
        .with_llm_content_export(state.export_llm_content(tenant.as_str()));
    if let Some(resolver) = secret_resolver {
        base_host = base_host.with_secret_resolver(resolver);
    }
    if let Some(interceptor) = local_blob_interceptor {
        base_host = base_host.with_binary_http_interceptor(interceptor);
    }

    let production_host: Arc<dyn WasmHost> = Arc::new(base_host);
    let local_host: Arc<dyn WasmHost> = Arc::new(LocalTDataWasmHost::new_for_wasm(
        state.clone(),
        tenant.clone(),
        &authz_context,
        production_host,
    ));
    Ok(Arc::new(AuthorizedWasmHost::new(
        local_host,
        gate,
        authz_context,
    )))
}

/// Shared context threaded through the WASM dispatch call chain.
///
/// Bundles the entity reference, trigger action, agent identity, and dispatch
/// mode so individual functions don't need to accept them as separate params.
struct WasmDispatchCtx<'a> {
    entity_ref: WasmEntityRef<'a>,
    action: &'a str,
    agent_ctx: &'a AgentContext,
    dispatch_idempotency_key: Option<&'a str>,
    mode: WasmDispatchMode,
}

fn agent_ctx_for_composite_wasm_result(
    agent_ctx: &AgentContext,
    dispatch_idempotency_key: Option<&str>,
) -> AgentContext {
    let mut composite_agent_ctx = agent_ctx.clone();
    if composite_agent_ctx.idempotency_key.is_none()
        && let Some(idempotency_key) = dispatch_idempotency_key
    {
        composite_agent_ctx.idempotency_key = Some(idempotency_key.to_string());
    }
    composite_agent_ctx
}

const HTTP_CALL_AUTHZ_DENIED_PREFIX: &str = "authorization denied for http_call";
const MONTY_REPL_MODULE: &str = "monty_repl";
const WASM_DISPATCH_PHASE_MODULE_CACHE: &str = "dispatch.wasm.phase.module_cache";
const WASM_DISPATCH_PHASE_REPLAY_INPUT_INJECTION: &str =
    "dispatch.wasm.phase.replay_input_injection";
const WASM_DISPATCH_PHASE_INVOCATION_CONTEXT_BUILD: &str =
    "dispatch.wasm.phase.invocation_context_build";
const WASM_DISPATCH_PHASE_BLOB_REF_HYDRATION: &str = "dispatch.wasm.phase.blob_ref_hydration";
const WASM_DISPATCH_PHASE_AUTHZ_SECRET_RESOLUTION: &str =
    "dispatch.wasm.phase.authz_secret_resolution";
const WASM_DISPATCH_PHASE_HOST_CHAIN_BUILD: &str = "dispatch.wasm.phase.host_chain_build";
const WASM_DISPATCH_PHASE_INTEGRATION_OBSERVE_START: &str =
    "dispatch.wasm.phase.integration_observe_start";
const WASM_DISPATCH_PHASE_ENGINE_INVOKE_AND_HANDLE: &str =
    "dispatch.wasm.phase.engine_invoke_and_handle";
const WASM_DISPATCH_PHASE_ENGINE_INVOKE: &str = "dispatch.wasm.phase.engine_invoke";
const WASM_DISPATCH_PHASE_RESULT_OBSERVE_COMPLETE: &str =
    "dispatch.wasm.phase.result_observe_complete";
const WASM_DISPATCH_PHASE_RECORD_INVOCATION: &str = "dispatch.wasm.phase.record_invocation";
const WASM_DISPATCH_PHASE_DISPATCH_CALLBACK: &str = "dispatch.wasm.phase.dispatch_callback";
const WASM_DISPATCH_PHASE_LLMOBS_SUBMIT: &str = "dispatch.wasm.phase.llmobs_submit";

fn http_call_authz_denied_error(reason: &str) -> String {
    format!("{HTTP_CALL_AUTHZ_DENIED_PREFIX}: {reason}")
}

fn is_http_call_authz_denial(error: &str) -> bool {
    error.contains(HTTP_CALL_AUTHZ_DENIED_PREFIX)
}

fn llmobs_service_name() -> String {
    for var in ["DD_SERVICE", "OTEL_SERVICE_NAME"] {
        let Some(value) = std::env::var(var) // determinism-ok: observability-only process config
            .ok()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
        else {
            continue;
        };
        return value;
    }
    "temper-platform".to_string()
}

fn wasm_dispatch_phase_slug(phase_name: &'static str) -> &'static str {
    phase_name
        .strip_prefix("dispatch.wasm.phase.")
        .unwrap_or(phase_name)
}

fn wasm_dispatch_phase_span(
    parent_span: &Span,
    ctx: &WasmDispatchCtx<'_>,
    module_name: &str,
    phase_name: &'static str,
) -> Span {
    let phase = wasm_dispatch_phase_slug(phase_name);
    tracing::info_span!(
        parent: parent_span,
        "dispatch.wasm.phase",
        otel.name = phase_name,
        phase = phase,
        tenant = %ctx.entity_ref.tenant,
        entity_type = ctx.entity_ref.entity_type,
        entity_id = ctx.entity_ref.entity_id,
        trigger_action = ctx.action,
        wasm.module = module_name,
        result = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
    )
}

fn record_wasm_dispatch_phase(span: &Span, started_at: Instant, result: &'static str) {
    span.record("duration_ms", started_at.elapsed().as_secs_f64() * 1_000.0);
    span.record("result", result);
}

fn with_wasm_dispatch_phase<T>(
    parent_span: &Span,
    ctx: &WasmDispatchCtx<'_>,
    module_name: &str,
    phase_name: &'static str,
    work: impl FnOnce() -> T,
) -> T {
    let span = wasm_dispatch_phase_span(parent_span, ctx, module_name, phase_name);
    let started_at = Instant::now(); // determinism-ok: observability-only span duration
    let _guard = span.enter();
    let output = work();
    drop(_guard);
    record_wasm_dispatch_phase(&span, started_at, "ok");
    output
}

async fn instrument_wasm_dispatch_phase<T, F>(
    parent_span: Span,
    ctx: &WasmDispatchCtx<'_>,
    module_name: &str,
    phase_name: &'static str,
    future: F,
) -> T
where
    F: Future<Output = T>,
{
    let span = wasm_dispatch_phase_span(&parent_span, ctx, module_name, phase_name);
    let started_at = Instant::now(); // determinism-ok: observability-only span duration
    let output = future.instrument(span.clone()).await;
    record_wasm_dispatch_phase(&span, started_at, "ok");
    output
}

async fn instrument_wasm_dispatch_phase_result<T, E, F>(
    parent_span: Span,
    ctx: &WasmDispatchCtx<'_>,
    module_name: &str,
    phase_name: &'static str,
    future: F,
) -> Result<T, E>
where
    F: Future<Output = Result<T, E>>,
{
    let span = wasm_dispatch_phase_span(&parent_span, ctx, module_name, phase_name);
    let started_at = Instant::now(); // determinism-ok: observability-only span duration
    let result = future.instrument(span.clone()).await;
    let status = if result.is_ok() { "ok" } else { "error" };
    record_wasm_dispatch_phase(&span, started_at, status);
    result
}

fn local_blob_binary_interceptor(
    state: crate::state::ServerState,
    tenant: TenantId,
    blob_endpoint: Option<String>,
) -> Option<BinaryHttpInterceptorFn> {
    let endpoint = crate::blob_store::LocalInternalBlobEndpoint::parse(&blob_endpoint?)?;
    Some(Arc::new(move |method, url, _headers, body| {
        let state = state.clone();
        let tenant = tenant.clone();
        let endpoint = endpoint.clone();
        Box::pin(async move {
            let blob_key = endpoint.object_key(&url)?;
            crate::runtime_metrics::record_blob_local_fast_path_request(&method);
            tracing::info!(
                method = %method,
                blob_key = %blob_key,
                "handling local blob request without loopback HTTP"
            );

            let result = match method.to_ascii_uppercase().as_str() {
                "PUT" => state
                    .put_blob_object(&tenant, &blob_key, &body, None)
                    .await
                    .map(|()| (204, Vec::new())),
                "GET" => state
                    .get_blob_with_legacy_fallback(&tenant, &blob_key)
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

pub(crate) fn internal_api_base_url(state: &crate::state::ServerState) -> Option<String> {
    std::env::var("TEMPER_API_URL") // determinism-ok: production host loopback config
        .ok()
        .map(|value| value.trim_end_matches('/').to_string())
        .filter(|value| !value.is_empty())
        .or_else(|| {
            state
                .listen_port
                .get()
                .copied()
                .map(|port| format!("http://127.0.0.1:{port}"))
        })
}

fn parse_internal_file_value_request(base_url: &str, url: &str) -> Option<String> {
    let prefix = format!("{}/tdata/Files('", base_url.trim_end_matches('/'));
    let remainder = url.strip_prefix(&prefix)?;
    let file_id = remainder.strip_suffix("')/$value")?;
    Some(file_id.replace("''", "'"))
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn local_file_value_text_interceptor(
    state: crate::state::ServerState,
    tenant: TenantId,
    agent_ctx: AgentContext,
    temper_api_url: Option<String>,
) -> Option<TextHttpInterceptorFn> {
    let base_url = temper_api_url?.trim_end_matches('/').to_string();
    let is_loopback = base_url.starts_with("http://127.0.0.1:")
        || base_url.starts_with("http://localhost:")
        || base_url.starts_with("http://[::1]:")
        || base_url.starts_with("https://localhost:");
    if !is_loopback {
        return None;
    }

    Some(Arc::new(
        move |method: String, url: String, headers: Vec<(String, String)>, body: String| {
            let state = state.clone();
            let tenant = tenant.clone();
            let agent_ctx = agent_ctx.clone();
            let base_url = base_url.clone();
            Box::pin(async move {
                let file_id = match parse_internal_file_value_request(&base_url, &url) {
                    Some(file_id) => file_id,
                    None => return None,
                };

                tracing::info!(
                    method = %method,
                    file_id = %file_id,
                    "handling internal File $value request without loopback HTTP"
                );

                match method.as_str() {
                    "GET" => {
                        let (status, bytes) = match state
                            .get_file_stream_content(&tenant, &file_id, &agent_ctx)
                            .await
                        {
                            Ok(result) => result,
                            Err(error) => return Some(Err(error)),
                        };
                        if status != 200 {
                            return Some(Ok((status, String::new())));
                        }
                        match String::from_utf8(bytes) {
                            Ok(text) => Some(Ok((200, text))),
                            Err(_) => None,
                        }
                    }
                    "PUT" => {
                        let content_type = header_value(&headers, "content-type")
                            .unwrap_or("application/octet-stream");
                        Some(
                            state
                                .put_file_stream_content(
                                    &tenant,
                                    &file_id,
                                    body.as_bytes(),
                                    content_type,
                                    &agent_ctx,
                                )
                                .await
                                .map(|_| (204, String::new())),
                        )
                    }
                    _ => None,
                }
            })
        },
    ))
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
            let spec = match req.agent_ctx.schema_pin.as_ref() {
                Some(pin) => registry.get_scoped_spec_at_digest(
                    req.tenant,
                    &pin.scope,
                    &pin.bundle_digest,
                    req.entity_type,
                ),
                None => registry.get_spec(req.tenant, req.entity_type),
            };
            spec.map(|spec| spec.integrations.clone())
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
        let blob_hydration_budget = crate::blobs::BlobHydrationBudget::wasm_dispatch();
        let blob_cache = instrument_wasm_dispatch_phase(
            active_parent_span.clone(),
            ctx,
            &module_name,
            WASM_DISPATCH_PHASE_BLOB_REF_HYDRATION,
            crate::blobs::hydrate_blob_refs_for_tenant_with_budget(
                self,
                ctx.entity_ref.tenant,
                &mut inv_ctx.entity_state,
                &blob_hydration_budget,
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
        let gate = crate::authz::wasm_gate::bind_local_blob_endpoint(
            gate,
            tenant_secrets.get("blob_endpoint").map(String::as_str),
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
                let internal_api_url = internal_api_base_url(self);
                let local_blob_interceptor = local_blob_binary_interceptor(
                    self.clone(),
                    ctx.entity_ref.tenant.clone(),
                    tenant_secrets.get("blob_endpoint").cloned(),
                );
                let local_file_interceptor = local_file_value_text_interceptor(
                    self.clone(),
                    ctx.entity_ref.tenant.clone(),
                    ctx.agent_ctx.clone(),
                    internal_api_url.clone(),
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
                let internal_capability_issuer =
                    internal_wasm_http_capability_issuer(self, ctx.entity_ref.tenant, &authz_ctx);
                let mut production_host_builder =
                    ProductionWasmHost::with_timeout(tenant_secrets, http_timeout)
                        .with_binary_http_interceptor(
                            local_blob_interceptor
                                .unwrap_or_else(|| Arc::new(|_, _, _, _| Box::pin(async { None }))),
                        )
                        .with_spec_evaluator(spec_evaluator_fn())
                        .with_progress_emitter(progress_emitter)
                        .with_internal_api_base_url(internal_api_url)
                        .with_invocation_context(host_invocation_context)
                        .with_llm_content_export(
                            self.export_llm_content(ctx.entity_ref.tenant.as_str()),
                        )
                        .with_text_http_interceptor(
                            local_file_interceptor
                                .unwrap_or_else(|| Arc::new(|_, _, _, _| Box::pin(async { None }))),
                        )
                        .with_trace_id(
                            current_otel_trace_id(active_span)
                                .or_else(|| ctx.agent_ctx.trace_id.clone()),
                        );
                production_host_builder = production_host_builder
                    .with_internal_capability_issuer(internal_capability_issuer);
                if let Some(resolver) = secret_resolver.clone() {
                    production_host_builder =
                        production_host_builder.with_secret_resolver(resolver);
                }
                let data_binding = self.wasm_module_registry.read().ok().and_then(|registry| {
                    registry
                        .data_manifest(ctx.entity_ref.tenant, &module_name, &hash)
                        .cloned()
                });
                if let (Some(binding), Some(security)) =
                    (data_binding.clone(), ctx.agent_ctx.security_ctx.clone())
                {
                    let budgets = binding.grant.budgets.clone();
                    let authority = ModuleInvocationAuthority::new(
                        ctx.entity_ref.tenant.clone(),
                        module_name.clone(),
                        hash.clone(),
                        ctx.action.to_string(),
                        ctx.entity_ref.entity_type.to_string(),
                        security,
                        binding,
                    );
                    let service = ApplicationDataInvocation::new(self.clone(), authority);
                    let (data, read, write) = service.callbacks();
                    production_host_builder = production_host_builder
                        .with_temper_data_service(data, read, write, &budgets);
                } else if let Some(binding) = data_binding {
                    let budgets = binding.grant.budgets.clone();
                    let data: TemperDataCallFn = Arc::new(|_| {
                        Box::pin(async {
                            serde_json::to_vec(&temper_wasm_sdk::data::DataResponseV1::error(
                                temper_wasm_sdk::data::ModuleDataError::new(
                                    temper_wasm_sdk::data::ModuleDataErrorKind::AuthorizationDenied,
                                    "AuthorizationDenied",
                                    "module invocation has no originating security context",
                                    temper_wasm_sdk::data::Retryability::Never,
                                ),
                            ))
                            .map_err(|error| error.to_string())
                        })
                    });
                    let read: TemperFileReadFn = Arc::new(|_, _| Err(-3));
                    let write: TemperFileWriteFn = Arc::new(|_, _| Err(-3));
                    production_host_builder = production_host_builder
                        .with_temper_data_service(data, read, write, &budgets);
                }
                let production_host: Arc<dyn WasmHost> = Arc::new(production_host_builder);
                let inner: Arc<dyn WasmHost> = Arc::new(LocalTDataWasmHost::new_for_wasm(
                    self.clone(),
                    ctx.entity_ref.tenant.clone(),
                    &authz_ctx,
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
            invoke_and_handle_result_boxed(
                self,
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
                .get_ots_trajectory(&row.tenant, &row.trajectory_id)
                .await
                .ok()
                .flatten()
            {
                Some(document) => document.data,
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

                // ARN-243: redact LLM content (prompt/completion/system/tool)
                // from the callback params unless this tenant opted into content
                // export. Stripping here covers every downstream telemetry sink
                // — the span record below, `llm_call_wide_event`,
                // `submit_llmobs_llm_span`, and `submit_llmobs_tool_spans` — all
                // of which read from these params. Metadata (tokens, model,
                // provider, finish reason, trace ids) is preserved. See ADR-0166.
                redact_llm_content_params(
                    &mut result.callback_params,
                    self.export_llm_content(ctx.entity_ref.tenant.as_str()),
                );
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
                    return handle_wasm_failure_boxed(
                        self,
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
                handle_wasm_failure_boxed(
                    self,
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
                handle_wasm_failure_boxed(
                    self,
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

    /// In-process `/tdata` host for `$value` / `blob_adapter`.
    ///
    /// Uses the HTTP caller. System is dropped so the guest cannot inherit
    /// `system-platform:broad-permit`.
    pub(crate) fn local_tdata_direct_host(
        &self,
        tenant: &TenantId,
        production_host: Arc<dyn WasmHost>,
        security_ctx: &SecurityContext,
    ) -> Arc<dyn WasmHost> {
        let loopback_ctx =
            (security_ctx.principal.kind != PrincipalKind::System).then_some(security_ctx);
        Arc::new(LocalTDataWasmHost::new(
            self.clone(),
            tenant.clone(),
            loopback_ctx,
            production_host,
        ))
    }

    /// Invoke a WASM module directly (not triggered by an entity action).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn invoke_wasm_direct(
        &self,
        tenant: &TenantId,
        module_name: &str,
        mut context: WasmInvocationContext,
        streams: Arc<std::sync::RwLock<StreamRegistry>>,
        security_ctx: &SecurityContext,
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
        let base_gate = crate::authz::wasm_gate::bind_local_blob_endpoint(
            base_gate,
            tenant_secrets.get("blob_endpoint").map(String::as_str),
        );
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
            .with_invocation_context(context.clone())
            .with_llm_content_export(self.export_llm_content(tenant.as_str()));
        if let Some(resolver) = secret_resolver {
            base_host = base_host.with_secret_resolver(resolver);
        }
        if let Some(interceptor) = local_blob_interceptor {
            base_host = base_host.with_binary_http_interceptor(interceptor);
        }
        let production_host: Arc<dyn WasmHost> = Arc::new(base_host);
        let inner = self.local_tdata_direct_host(tenant, production_host, security_ctx);
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
    let provider = llm_provider_for_observability(entity_state, callback_params);
    let model = llm_model_for_observability(entity_state, callback_params);
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
    let system_instructions = callback_params
        .get("_gen_ai_system_instructions")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let trace_id = current_otel_trace_id(&Span::current())
        .or_else(|| ctx.agent_ctx.trace_id.clone())
        .unwrap_or_default();

    temper_observe::wide_event::from_llm_call(temper_observe::wide_event::LlmCallInput {
        provider: &provider,
        model: &model,
        operation: "chat",
        entity_type: ctx.entity_ref.entity_type,
        entity_id: ctx.entity_ref.entity_id,
        session_id,
        success: true,
        duration_ns: duration_ms * 1_000_000,
        input_tokens,
        output_tokens,
        stop_reason,
        system_instructions,
        input_messages,
        output_messages,
        trace_id: &trace_id,
        error: None,
    })
}

fn should_record_gen_ai_span_attrs(integration_is_llm: bool, _callback_params: &Value) -> bool {
    integration_is_llm
}

fn llm_provider_for_observability(entity_state: &EntityState, callback_params: &Value) -> String {
    let provider = callback_params
        .get("_gen_ai_provider")
        .and_then(Value::as_str)
        .or_else(|| entity_state.fields.get("provider").and_then(Value::as_str))
        .unwrap_or("unknown");
    normalize_llm_provider_for_observability(provider)
}

fn normalize_llm_provider_for_observability(provider: &str) -> String {
    let trimmed = provider.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "openai_codex" => "openai".to_string(),
        "mock" => "custom".to_string(),
        "" => "unknown".to_string(),
        _ => trimmed.to_string(),
    }
}

fn llm_model_for_observability(entity_state: &EntityState, callback_params: &Value) -> String {
    callback_params
        .get("_gen_ai_model")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            entity_state
                .fields
                .get("model")
                .and_then(Value::as_str)
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or("unknown")
        .trim()
        .to_string()
}

async fn submit_llmobs_llm_span(
    ctx: &WasmDispatchCtx<'_>,
    entity_state: &EntityState,
    callback_params: &Value,
    duration_ms: u64,
    module_name: &str,
) {
    let current_trace_id = current_otel_trace_id(&Span::current());
    let trace_id = callback_params
        .get("_gen_ai_parent_trace_id")
        .and_then(Value::as_str)
        .or(current_trace_id.as_deref());
    let span_id = callback_params
        .get("_gen_ai_parent_span_id")
        .and_then(Value::as_str);
    let parent_span_id = callback_params
        .get("_gen_ai_llm_parent_span_id")
        .and_then(Value::as_str)
        .or(ctx.agent_ctx.parent_span_id.as_deref());
    let (Some(trace_id), Some(span_id)) = (trace_id, span_id) else {
        return;
    };

    let provider = llm_provider_for_observability(entity_state, callback_params);
    let model = llm_model_for_observability(entity_state, callback_params);
    let session_id = ctx
        .agent_ctx
        .session_id
        .as_deref()
        .unwrap_or(ctx.entity_ref.entity_id);
    let span_name = format!("wasm:{module_name}");
    let workflow_name = format!("{}.{}", ctx.entity_ref.entity_type, ctx.action);
    let agent_span_id = callback_params
        .get("_gen_ai_llmobs_agent_span_id")
        .and_then(Value::as_str)
        .or(parent_span_id);
    let workflow_span_id = callback_params
        .get("_gen_ai_llmobs_workflow_span_id")
        .and_then(Value::as_str);

    let input_tokens = callback_params
        .get("input_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let output_tokens = callback_params
        .get("output_tokens")
        .and_then(Value::as_i64)
        .unwrap_or_default();
    let service_name = llmobs_service_name();

    if let Err(error) =
        temper_observe::llmobs_api::submit_llm_span(temper_observe::llmobs_api::LlmSpanInput {
            service_name: &service_name,
            session_id,
            trace_id,
            span_id,
            parent_span_id,
            agent_span_id,
            agent_start_ns: llmobs_agent_start_ns(entity_state, callback_params),
            workflow_span_id,
            agent_name: Some("temperpaw.agent.session"),
            workflow_name: Some(&workflow_name),
            span_name: &span_name,
            provider: &provider,
            model: &model,
            system_instructions: callback_params
                .get("_gen_ai_system_instructions")
                .and_then(Value::as_str),
            input_messages_json: callback_params
                .get("_gen_ai_input_messages")
                .and_then(Value::as_str),
            output_messages_json: callback_params
                .get("_gen_ai_output_messages")
                .and_then(Value::as_str),
            input_tokens,
            output_tokens,
            finish_reason: callback_params
                .get("_gen_ai_finish_reason")
                .and_then(Value::as_str),
            duration_ms,
            error_type: None,
        })
        .await
    {
        tracing::warn!(
            tenant = %ctx.entity_ref.tenant,
            entity_id = ctx.entity_ref.entity_id,
            session_id,
            %error,
            "failed to submit llm span to Datadog LLM Observability API"
        );
    }
}

async fn submit_llmobs_tool_spans(
    ctx: &WasmDispatchCtx<'_>,
    entity_state: &EntityState,
    callback_params: &Value,
) {
    let raw_events = callback_params
        .get("_dd_llmobs_tool_spans")
        .and_then(Value::as_array);
    let Some(raw_events) = raw_events else {
        return;
    };

    let Some((trace_id, parent_span_id)) =
        llmobs_tool_trace_and_parent(entity_state, callback_params)
    else {
        tracing::warn!(
            tenant = %ctx.entity_ref.tenant,
            entity_id = ctx.entity_ref.entity_id,
            raw_event_count = raw_events.len(),
            "skipping Datadog LLMObs tool span submission because parent trace context is missing"
        );
        return;
    };
    let session_id = ctx
        .agent_ctx
        .session_id
        .as_deref()
        .unwrap_or(ctx.entity_ref.entity_id);
    let service_name = llmobs_service_name();

    let spans: Vec<_> = raw_events
        .iter()
        .filter_map(|event| {
            let tool_name = event.get("tool_name").and_then(Value::as_str)?;
            let tool_call_id = event.get("tool_call_id").and_then(Value::as_str)?;
            let arguments = event
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            let result_text = event.get("result").and_then(Value::as_str).unwrap_or("");
            let duration_ms = event
                .get("duration_ms")
                .and_then(Value::as_u64)
                .unwrap_or_default();
            let is_error = event
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            Some(temper_observe::llmobs_api::ToolSpanInput {
                service_name: &service_name,
                session_id,
                trace_id: &trace_id,
                parent_span_id: &parent_span_id,
                tool_name,
                tool_call_id,
                arguments_json: arguments,
                result_text,
                duration_ms,
                is_error,
            })
        })
        .collect();

    if spans.is_empty() {
        return;
    }

    if let Err(error) = temper_observe::llmobs_api::submit_tool_spans(
        &service_name,
        session_id,
        &trace_id,
        &parent_span_id,
        &spans,
    )
    .await
    {
        tracing::warn!(
            tenant = %ctx.entity_ref.tenant,
            entity_id = ctx.entity_ref.entity_id,
            session_id,
            %error,
            "failed to submit tool spans to Datadog LLM Observability API"
        );
    }
}

fn llmobs_tool_trace_and_parent(
    entity_state: &EntityState,
    callback_params: &Value,
) -> Option<(String, String)> {
    let trace_id = entity_state
        .fields
        .get("gen_ai_parent_trace_id")
        .and_then(Value::as_str)
        .or_else(|| {
            entity_state
                .fields
                .get("_gen_ai_parent_trace_id")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            callback_params
                .get("_gen_ai_parent_trace_id")
                .and_then(Value::as_str)
        })?;
    let parent_span_id = entity_state
        .fields
        .get("llmobs_workflow_span_id")
        .and_then(Value::as_str)
        .or_else(|| {
            entity_state
                .fields
                .get("_gen_ai_llmobs_workflow_span_id")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            callback_params
                .get("_gen_ai_llmobs_workflow_span_id")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            entity_state
                .fields
                .get("gen_ai_parent_span_id")
                .and_then(Value::as_str)
        })
        .or_else(|| {
            entity_state
                .fields
                .get("_gen_ai_parent_span_id")
                .and_then(Value::as_str)
        })?;

    Some((trace_id.to_string(), parent_span_id.to_string()))
}

fn current_otel_trace_id(span: &Span) -> Option<String> {
    let span_context = span.context().span().span_context().clone();
    if span_context.is_valid() {
        Some(span_context.trace_id().to_string())
    } else {
        None
    }
}

fn current_otel_span_id(span: &Span) -> Option<String> {
    let span_context = span.context().span().span_context().clone();
    if span_context.is_valid() {
        Some(span_context.span_id().to_string())
    } else {
        None
    }
}

pub(super) fn record_wasm_error_on_current_span(error: &str) {
    let span = Span::current();
    record_wasm_error_on_span(&span, error);
}

fn record_wasm_error_on_span(span: &Span, error: &str) {
    let error_type = integration_error_type(error);
    span.record("error.type", error_type.as_str());
    span.record("error.message", error);
    span.record("exception.message", error);
    span.set_status(Status::error(error.to_string()));
}

fn build_llm_root_span(
    ctx: &WasmDispatchCtx<'_>,
    integration: &temper_spec::automaton::Integration,
    entity_state: &EntityState,
    module_name: &str,
) -> Span {
    let provider = entity_state
        .fields
        .get("provider")
        .and_then(|v| v.as_str())
        .unwrap_or("anthropic");
    let provider = normalize_llm_provider_for_observability(provider);
    let model = entity_state
        .fields
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let session_id = ctx
        .agent_ctx
        .session_id
        .as_deref()
        .unwrap_or(ctx.entity_ref.entity_id);

    let span = tracing::info_span!(
        "llm_caller.trace",
        otel.name = %format!("wasm:{module_name}"),
        integration = %integration.name,
        wasm.module = %module_name,
        gen_ai.system = %provider,
        gen_ai.provider.name = %provider,
        dd_llmobs_enabled = false,
        gen_ai.system_instructions = tracing::field::Empty,
        gen_ai.request.model = %model,
        gen_ai.operation.name = "chat",
        gen_ai.response.finish_reasons = tracing::field::Empty,
        gen_ai.usage.input_tokens = tracing::field::Empty,
        gen_ai.usage.output_tokens = tracing::field::Empty,
        gen_ai.conversation.id = %session_id,
        gen_ai.input.messages = tracing::field::Empty,
        gen_ai.output.messages = tracing::field::Empty,
        error.type = tracing::field::Empty,
        error.message = tracing::field::Empty,
        exception.message = tracing::field::Empty,
    );
    span
}

fn attach_llm_parent_context(
    span: &Span,
    llm_parent_span_id: Option<&str>,
    entity_state: &EntityState,
    session_id: &str,
    duration_ms: u64,
    callback_params: &mut Value,
) {
    let span_context = span.context().span().span_context().clone();
    if !span_context.is_valid() {
        return;
    }

    let Some(object) = callback_params.as_object_mut() else {
        return;
    };

    object.insert(
        "_gen_ai_parent_trace_id".into(),
        json!(span_context.trace_id().to_string()),
    );
    object.insert(
        "gen_ai_parent_trace_id".into(),
        json!(span_context.trace_id().to_string()),
    );
    object.insert(
        "_gen_ai_parent_span_id".into(),
        json!(span_context.span_id().to_string()),
    );
    object.insert(
        "gen_ai_parent_span_id".into(),
        json!(span_context.span_id().to_string()),
    );
    if let Some(parent_span_id) = llm_parent_span_id
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        object.insert(
            "_gen_ai_llm_parent_span_id".into(),
            json!(parent_span_id.to_string()),
        );
        object.insert(
            "gen_ai_llm_parent_span_id".into(),
            json!(parent_span_id.to_string()),
        );
    }

    let trace_id = span_context.trace_id().to_string();
    let agent_span_id = entity_state
        .fields
        .get("llmobs_agent_span_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            temper_observe::llmobs_api::derive_span_id(&format!("{trace_id}:{session_id}:agent"))
        });
    object.insert(
        "_gen_ai_llmobs_agent_span_id".into(),
        json!(agent_span_id.clone()),
    );
    object.insert("llmobs_agent_span_id".into(), json!(agent_span_id));

    let agent_start_ns = entity_state
        .fields
        .get("llmobs_agent_start_ns")
        .and_then(value_as_u64)
        .unwrap_or_else(|| llmobs_agent_start_ns_for_duration(duration_ms));
    object.insert(
        "_gen_ai_llmobs_agent_start_ns".into(),
        json!(agent_start_ns),
    );
    object.insert("llmobs_agent_start_ns".into(), json!(agent_start_ns));

    let workflow_span_id = temper_observe::llmobs_api::derive_span_id(&format!(
        "{}:{}:workflow",
        span_context.trace_id(),
        span_context.span_id()
    ));
    object.insert(
        "_gen_ai_llmobs_workflow_span_id".into(),
        json!(workflow_span_id.clone()),
    );
    object.insert("llmobs_workflow_span_id".into(), json!(workflow_span_id));
}

fn llmobs_agent_start_ns(entity_state: &EntityState, callback_params: &Value) -> Option<u64> {
    callback_params
        .get("_gen_ai_llmobs_agent_start_ns")
        .and_then(value_as_u64)
        .or_else(|| {
            callback_params
                .get("llmobs_agent_start_ns")
                .and_then(value_as_u64)
        })
        .or_else(|| {
            entity_state
                .fields
                .get("llmobs_agent_start_ns")
                .and_then(value_as_u64)
        })
}

fn llmobs_agent_start_ns_for_duration(duration_ms: u64) -> u64 {
    current_unix_ns().saturating_sub(duration_ms.saturating_add(100).saturating_mul(1_000_000))
}

fn current_unix_ns() -> u64 {
    SystemTime::now() // determinism-ok: LLM observability timestamp translation
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64
}

fn value_as_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
}

fn strip_private_observability_params(mut params: Value) -> Value {
    let Some(object) = params.as_object_mut() else {
        return params;
    };

    object.retain(|key, _| !key.starts_with("_gen_ai_") && key.as_str() != "_dd_llmobs_tool_spans");
    params
}

/// Callback-param keys that carry LLM *content* (prompt, completion, system
/// prompt, and tool arguments/results) rather than safe metadata. These are the
/// keys the telemetry sinks read — the span record, [`llm_call_wide_event`],
/// [`submit_llmobs_llm_span`], and [`submit_llmobs_tool_spans`] — so stripping
/// them from `callback_params` redacts content across every sink at once.
/// The gate is an allowlist over [`is_private_llm_observability_param`], so these
/// are not what enforces redaction — they are the explicit statement of which
/// params are content, used by the test that proves the allowlist drops each one.
/// See ADR-0166.
#[cfg(test)]
const LLM_CONTENT_PARAM_KEYS: [&str; 4] = [
    "_gen_ai_input_messages",
    "_gen_ai_output_messages",
    "_gen_ai_system_instructions",
    "_dd_llmobs_tool_spans",
];

/// Callback-param keys the sinks record under `gen_ai.*` semantic-convention
/// names. Their values come from the guest, so a key name cannot establish that
/// the value is metadata: a module for a non-opted-in tenant that returns
/// `{"_gen_ai_model": "<the whole prompt>"}` would otherwise reach LLM
/// Observability as `gen_ai.request.model`. They are kept, but bounded — the same
/// rule the other three channels apply. See ADR-0166.
const LLM_METADATA_PARAM_KEYS: [&str; 8] = [
    "_gen_ai_provider",
    "_gen_ai_model",
    "_gen_ai_finish_reason",
    "_gen_ai_parent_trace_id",
    "_gen_ai_parent_span_id",
    "_gen_ai_llm_parent_span_id",
    "_gen_ai_llmobs_agent_span_id",
    "_gen_ai_llmobs_workflow_span_id",
];

/// Redact LLM content params from `callback_params` unless the tenant has opted
/// into LLM content export. Removes only the content keys in
/// [`LLM_CONTENT_PARAM_KEYS`]; metadata is preserved. No-op when
/// `export_content` is true. See ADR-0166.
/// Whether a callback param is a private LLM-observability channel (the `_gen_ai_`
/// and `_dd_llmobs_` prefixes the telemetry sinks read) rather than ordinary
/// action output. Prefix-based so a param added later is governed by default
/// instead of silently exempt.
fn is_private_llm_observability_param(key: &str) -> bool {
    // Case-insensitive, matching the normalization the other channels apply.
    // Today's sinks look these up with exact lowercase names, so `_GEN_AI_prompt`
    // is not exported — but it would sit in the map looking governed, waiting for
    // the first sink that folds case. Cheaper to normalize than to rely on that.
    let key = key.to_ascii_lowercase();
    key.starts_with("_gen_ai_") || key.starts_with("_dd_llmobs_")
}

fn redact_llm_content_params(callback_params: &mut Value, export_content: bool) {
    if export_content {
        return;
    }
    let Some(object) = callback_params.as_object_mut() else {
        return;
    };
    // Allowlist, not denylist. Removing four known content keys leaves any *other*
    // observability param — `_gen_ai_completion`, say — in the map for a sink
    // added later to pick up. The other channels were switched to an allowlist for
    // exactly this reason; this one is now consistent with them: a private
    // observability param survives only if it is recognised metadata.
    object.retain(|key, _| {
        !is_private_llm_observability_param(key)
            || LLM_METADATA_PARAM_KEYS.contains(&key.to_ascii_lowercase().as_str())
    });
    for key in LLM_METADATA_PARAM_KEYS {
        let Some(Value::String(text)) = object.get_mut(key) else {
            continue;
        };
        if let Some(clamped) = clamp_redacted_metadata_value(text) {
            *text = clamped;
        }
    }
}

fn integration_error_type(error: &str) -> String {
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

#[cfg(test)]
#[path = "wasm/llm_redaction_test.rs"]
mod llm_redaction_test;
#[cfg(test)]
#[path = "wasm/wasm_test.rs"]
mod tests;
