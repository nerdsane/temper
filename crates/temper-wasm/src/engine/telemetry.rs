use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use wasmtime::Store;

use crate::metrics;
use crate::stream::StreamRegistry;
use crate::types::{WasmInvocationContext, WasmInvocationResult};

use super::{HostState, WasmError};

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
            WasmError::Invocation(err)
        }
    }
}

pub(super) fn empty_result(
    context: &WasmInvocationContext,
    needs_wasi: bool,
    duration_ms: u64,
) -> WasmInvocationResult {
    metrics::record_wasm_invoke(
        &context.entity_type,
        &context.trigger_action,
        needs_wasi,
        false,
        duration_ms as f64,
    );
    WasmInvocationResult {
        callback_action: String::new(),
        callback_params: serde_json::Value::Null,
        success: false,
        error: Some("module returned empty result".to_string()),
        duration_ms,
    }
}

pub(super) fn parse_result_json(
    result_json: &str,
    context: &WasmInvocationContext,
    needs_wasi: bool,
    duration_ms: u64,
) -> Result<serde_json::Value, WasmError> {
    serde_json::from_str(result_json).map_err(|error| {
        metrics::record_wasm_invoke(
            &context.entity_type,
            &context.trigger_action,
            needs_wasi,
            false,
            duration_ms as f64,
        );
        WasmError::Invocation(format!("failed to parse result JSON: {error}"))
    })
}

pub(super) fn finalize_result(
    store: &Store<HostState>,
    parsed: serde_json::Value,
    context: &WasmInvocationContext,
    needs_wasi: bool,
    duration_ms: u64,
) -> WasmInvocationResult {
    let callback_action = parsed
        .get("action")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let success = parsed
        .get("success")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let error = parsed
        .get("error")
        .and_then(|v| v.as_str())
        .map(String::from);
    let stream_count_after = store
        .data()
        .streams
        .read()
        .map(|registry| registry.stream_count() as u64)
        .unwrap_or_default();
    tracing::Span::current().record("stream_count_after", stream_count_after);
    tracing::Span::current().record("success", success);
    tracing::Span::current().record("callback_action", callback_action.as_str());
    if let Some(ref error_message) = error {
        tracing::Span::current().record("error", error_message.as_str());
    }
    metrics::record_wasm_invoke(
        &context.entity_type,
        &context.trigger_action,
        needs_wasi,
        success,
        duration_ms as f64,
    );

    WasmInvocationResult {
        callback_action,
        callback_params: parsed
            .get("params")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
        success,
        error,
        duration_ms,
    }
}

fn record_failure(
    context: &WasmInvocationContext,
    needs_wasi: bool,
    duration_ms: f64,
    error: &str,
) {
    tracing::Span::current().record("success", false);
    tracing::Span::current().record("error", error);
    metrics::record_wasm_invoke(
        &context.entity_type,
        &context.trigger_action,
        needs_wasi,
        false,
        duration_ms,
    );
}
