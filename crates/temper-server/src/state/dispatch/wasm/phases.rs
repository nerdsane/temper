//! Phase-tracing helpers for the WASM dispatch pipeline.
//!
//! Every stage of `dispatch_single_integration` is wrapped in a child span
//! carrying timing and result attributes so traces show where dispatch time
//! is spent. These helpers are observability-only and never alter dispatch
//! behavior.

use std::future::Future;
use std::time::Instant;

use tracing::{Instrument, Span};

use super::WasmDispatchCtx;

pub(super) const WASM_DISPATCH_PHASE_MODULE_CACHE: &str = "dispatch.wasm.phase.module_cache";
pub(super) const WASM_DISPATCH_PHASE_REPLAY_INPUT_INJECTION: &str =
    "dispatch.wasm.phase.replay_input_injection";
pub(super) const WASM_DISPATCH_PHASE_INVOCATION_CONTEXT_BUILD: &str =
    "dispatch.wasm.phase.invocation_context_build";
pub(super) const WASM_DISPATCH_PHASE_BLOB_REF_HYDRATION: &str =
    "dispatch.wasm.phase.blob_ref_hydration";
pub(super) const WASM_DISPATCH_PHASE_AUTHZ_SECRET_RESOLUTION: &str =
    "dispatch.wasm.phase.authz_secret_resolution";
pub(super) const WASM_DISPATCH_PHASE_HOST_CHAIN_BUILD: &str =
    "dispatch.wasm.phase.host_chain_build";
pub(super) const WASM_DISPATCH_PHASE_INTEGRATION_OBSERVE_START: &str =
    "dispatch.wasm.phase.integration_observe_start";
pub(super) const WASM_DISPATCH_PHASE_ENGINE_INVOKE_AND_HANDLE: &str =
    "dispatch.wasm.phase.engine_invoke_and_handle";
pub(super) const WASM_DISPATCH_PHASE_ENGINE_INVOKE: &str = "dispatch.wasm.phase.engine_invoke";
pub(super) const WASM_DISPATCH_PHASE_RESULT_OBSERVE_COMPLETE: &str =
    "dispatch.wasm.phase.result_observe_complete";
pub(super) const WASM_DISPATCH_PHASE_RECORD_INVOCATION: &str =
    "dispatch.wasm.phase.record_invocation";
pub(super) const WASM_DISPATCH_PHASE_DISPATCH_CALLBACK: &str =
    "dispatch.wasm.phase.dispatch_callback";
pub(super) const WASM_DISPATCH_PHASE_LLMOBS_SUBMIT: &str = "dispatch.wasm.phase.llmobs_submit";

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

pub(super) fn with_wasm_dispatch_phase<T>(
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

pub(super) async fn instrument_wasm_dispatch_phase<T, F>(
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

pub(super) async fn instrument_wasm_dispatch_phase_result<T, E, F>(
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
