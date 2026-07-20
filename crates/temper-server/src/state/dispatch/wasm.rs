use std::future::Future;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use opentelemetry::trace::{Status, TraceContextExt};
use serde_json::{Value, json};
use tracing::{Instrument, Span, instrument};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::entity_actor::{EntityResponse, EntityState};
use crate::request_context::AgentContext;
use crate::secrets::template::resolve_secret_templates;
use crate::state::sim_now;
use temper_runtime::tenant::TenantId;
use temper_wasm::{
    AuthorizedWasmHost, BinaryHttpInterceptorFn, ProductionWasmHost, ProgressEmitterFn,
    StreamRegistry, TextHttpInterceptorFn, WasmAuthzContext, WasmAuthzGate, WasmHost,
    WasmInvocationContext, WasmResourceLimits,
};

use super::{
    HttpCallAuthzDenialTracker, TrackingWasmAuthzGate, WasmDispatchMode, WasmDispatchRequest,
    WasmEntityRef, record_workflow_span_attrs,
};
use replay_inputs::{extract_trajectory_actions_from_ots, has_replay_trajectory_input};

mod invocation_artifacts;
mod local_tdata_host;
mod replay_inputs;

use local_tdata_host::LocalTDataWasmHost;

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
    let endpoint = blob_endpoint?;
    if !crate::blob_store::is_local_internal_blob_endpoint(&endpoint) {
        return None;
    }

    let endpoint = endpoint.trim_end_matches('/').to_string();
    Some(Arc::new(move |method, url, _headers, body| {
        let state = state.clone();
        let tenant = tenant.clone();
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

fn internal_api_base_url(state: &crate::state::ServerState) -> Option<String> {
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

mod callbacks;
mod dispatch;
mod invocation;
mod llmobs;
mod observability;
mod ots;

use callbacks::*;
use llmobs::*;
use observability::*;

#[cfg(test)]
mod tests;
