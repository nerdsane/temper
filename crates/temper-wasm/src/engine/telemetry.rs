use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use opentelemetry::trace::Status;
use tracing_opentelemetry::OpenTelemetrySpanExt;
use wasmtime::Store;

use crate::metrics;
use crate::stream::StreamRegistry;
use crate::types::{WasmInvocationContext, WasmInvocationResult};

use super::{HostState, InvalidGuestResultKind, WasmError, result};

pub(super) fn record_invocation_start(
    context: &WasmInvocationContext,
    needs_wasi: bool,
    streams: &Arc<RwLock<StreamRegistry>>,
) {
    tracing::Span::current().record("needs_wasi", needs_wasi);
    if let Some(agent_id) = context.agent_id.as_deref() {
        tracing::Span::current().record("agent_id", agent_id);
    }
    if let Some(session_id) = context.session_id.as_deref() {
        tracing::Span::current().record("session_id", session_id);
    }
    let stream_count_before = streams
        .read()
        .map(|registry| registry.stream_count() as u64)
        .unwrap_or_default();
    tracing::Span::current().record("stream_count_before", stream_count_before);
}

pub(super) fn map_invoke_error(
    error: wasmtime::Error,
    context: &WasmInvocationContext,
    needs_wasi: bool,
    max_duration: Duration,
    started: Instant,
) -> WasmError {
    let duration_ms = started.elapsed().as_millis() as f64;
    match error.downcast_ref::<wasmtime::Trap>() {
        Some(&wasmtime::Trap::OutOfFuel) => {
            record_failure(context, needs_wasi, duration_ms, "fuel_exhausted");
            WasmError::FuelExhausted
        }
        Some(&wasmtime::Trap::Interrupt) => {
            record_failure(context, needs_wasi, duration_ms, "timeout");
            WasmError::Timeout(max_duration)
        }
        _ => {
            let err = error.to_string();
            record_failure(context, needs_wasi, duration_ms, err.as_str());
            WasmError::GuestExecution(err)
        }
    }
}

pub(super) fn parse_result_json(
    result_json: &str,
    context: &WasmInvocationContext,
    needs_wasi: bool,
    duration_ms: u64,
) -> Result<WasmInvocationResult, WasmError> {
    result::decode_terminal_result(result_json, duration_ms).map_err(|kind| {
        record_invalid_result(context, needs_wasi, duration_ms as f64, kind);
        WasmError::InvalidGuestResult(kind)
    })
}

pub(super) fn finalize_result(
    store: &Store<HostState>,
    result: WasmInvocationResult,
    context: &WasmInvocationContext,
    needs_wasi: bool,
) -> WasmInvocationResult {
    let stream_count_after = store
        .data()
        .streams
        .read()
        .map(|registry| registry.stream_count() as u64)
        .unwrap_or_default();
    tracing::Span::current().record("stream_count_after", stream_count_after);
    tracing::Span::current().record("success", result.success);
    tracing::Span::current().record("callback_action", result.callback_action.as_str());
    if let Some(ref error_message) = result.error {
        tracing::Span::current().record("error", error_message.as_str());
    }
    if !result.success {
        let typed_code = result
            .typed_failure
            .as_ref()
            .map(|failure| failure.code.as_str());
        let error_message = typed_code
            .or(result.error.as_deref())
            .unwrap_or("module returned unsuccessful result");
        let error_type = wasm_error_type(error_message);
        tracing::Span::current().record("error.type", error_type);
        tracing::Span::current().record("error.message", error_message);
        tracing::Span::current().record("exception.message", error_message);
        tracing::Span::current().set_status(Status::error(error_message.to_string()));
    }
    metrics::record_wasm_invoke(
        &context.entity_type,
        &context.trigger_action,
        needs_wasi,
        result.success,
        result.duration_ms as f64,
    );
    result
}

fn record_invalid_result(
    context: &WasmInvocationContext,
    needs_wasi: bool,
    duration_ms: f64,
    kind: InvalidGuestResultKind,
) {
    record_failure(context, needs_wasi, duration_ms, kind.source_code());
}

fn record_failure(
    context: &WasmInvocationContext,
    needs_wasi: bool,
    duration_ms: f64,
    error: &str,
) {
    let error_type = wasm_error_type(error);
    tracing::Span::current().record("success", false);
    tracing::Span::current().record("error", error);
    tracing::Span::current().record("error.type", error_type);
    tracing::Span::current().record("error.message", error);
    tracing::Span::current().record("exception.message", error);
    tracing::Span::current().set_status(Status::error(error.to_string()));
    metrics::record_wasm_invoke(
        &context.entity_type,
        &context.trigger_action,
        needs_wasi,
        false,
        duration_ms,
    );
}

fn wasm_error_type(error: &str) -> &'static str {
    let normalized = error.to_ascii_lowercase();
    if normalized.contains("timeout") {
        "timeout"
    } else if normalized.contains("fuel") {
        "fuel_exhausted"
    } else if normalized.contains("memory") {
        "memory_limit_exceeded"
    } else {
        "wasm_invocation_error"
    }
}
