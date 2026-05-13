use std::collections::BTreeMap;

use opentelemetry::KeyValue;
use opentelemetry::trace::TraceContextExt as _;
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::types::WasmInvocationContext;

use super::{context_session_id, current_log_correlation};

pub(super) fn record_guest_progress_event(
    event_json: &str,
    context: Option<&WasmInvocationContext>,
    error: Option<&str>,
) {
    let payload = serde_json::from_str::<Value>(event_json).unwrap_or(Value::Null);
    let progress_kind = payload
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("progress");
    let workflow_step = payload
        .get("workflow_step")
        .and_then(Value::as_str)
        .or_else(|| payload.get("step").and_then(Value::as_str))
        .unwrap_or(progress_kind);
    let tool_name = payload
        .get("tool_name")
        .and_then(Value::as_str)
        .unwrap_or("");
    let success = error.is_none();
    let error_message = error.unwrap_or("");

    let attrs = guest_progress_span_attrs(
        progress_kind,
        workflow_step,
        tool_name,
        success,
        context,
        error,
    )
    .into_iter()
    .map(|(key, value)| KeyValue::new(key, value))
    .collect::<Vec<_>>();
    let span = tracing::Span::current();
    let cx = span.context();
    let otel_span = cx.span();
    if otel_span.span_context().is_valid() {
        otel_span.add_event("wasm_guest.progress", attrs);
    }

    let tenant = context.map(|ctx| ctx.tenant.as_str()).unwrap_or("");
    let entity_type = context.map(|ctx| ctx.entity_type.as_str()).unwrap_or("");
    let entity_id = context.map(|ctx| ctx.entity_id.as_str()).unwrap_or("");
    let trigger_action = context.map(|ctx| ctx.trigger_action.as_str()).unwrap_or("");
    let agent_id = context
        .and_then(|ctx| ctx.agent_id.as_deref())
        .unwrap_or("");
    let wasm_module = context
        .and_then(|ctx| ctx.wasm_module.as_deref())
        .unwrap_or("");
    let session_id = context.and_then(context_session_id).unwrap_or("");
    let workflow_root_entity_type = context
        .and_then(|ctx| ctx.workflow_root_entity_type.as_deref())
        .unwrap_or("");
    let workflow_root_entity_id = context
        .and_then(|ctx| ctx.workflow_root_entity_id.as_deref())
        .unwrap_or("");
    let workflow_run_id = context
        .and_then(|ctx| ctx.workflow_run_id.as_deref())
        .unwrap_or("");
    let correlation = current_log_correlation(context.map(|ctx| ctx.trace_id.as_str()));

    tracing::event!(
        name: "wasm_guest.progress",
        target: "wasm_guest",
        tracing::Level::INFO,
        progress.kind = %progress_kind,
        workflow_step = %workflow_step,
        tool.name = %tool_name,
        success = success,
        tenant = %tenant,
        entity_type = %entity_type,
        entity_id = %entity_id,
        trigger_action = %trigger_action,
        action_name = %trigger_action,
        agent_id = %agent_id,
        wasm_module = %wasm_module,
        session_id = %session_id,
        gen_ai.conversation.id = %session_id,
        workflow_root_entity_type = %workflow_root_entity_type,
        workflow_root_entity_id = %workflow_root_entity_id,
        workflow_run_id = %workflow_run_id,
        trace_id = %correlation.trace_id,
        span_id = %correlation.span_id,
        otel.trace_id = %correlation.trace_id,
        otel.span_id = %correlation.span_id,
        dd.trace_id = %correlation.dd_trace_id,
        dd.span_id = %correlation.dd_span_id,
        error.message = %error_message,
        progress.fields_json = %event_json,
        "WASM guest progress",
    );
}

fn guest_progress_span_attrs(
    progress_kind: &str,
    workflow_step: &str,
    tool_name: &str,
    success: bool,
    context: Option<&WasmInvocationContext>,
    error: Option<&str>,
) -> BTreeMap<String, String> {
    let mut attrs = BTreeMap::new();
    attrs.insert("progress.kind".to_string(), progress_kind.to_string());
    attrs.insert("workflow_step".to_string(), workflow_step.to_string());
    attrs.insert("success".to_string(), success.to_string());
    if !tool_name.is_empty() {
        attrs.insert("tool.name".to_string(), tool_name.to_string());
    }
    if let Some(error) = error.filter(|value| !value.is_empty()) {
        attrs.insert("error.message".to_string(), error.to_string());
    }
    if let Some(context) = context {
        attrs.insert("tenant".to_string(), context.tenant.clone());
        attrs.insert("entity_type".to_string(), context.entity_type.clone());
        attrs.insert("entity_id".to_string(), context.entity_id.clone());
        attrs.insert("trigger_action".to_string(), context.trigger_action.clone());
        if let Some(module) = context
            .wasm_module
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            attrs.insert("wasm_module".to_string(), module.to_string());
        }
        if let Some(agent_id) = context
            .agent_id
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            attrs.insert("agent_id".to_string(), agent_id.to_string());
        }
        if let Some(session_id) = context_session_id(context) {
            attrs.insert("session_id".to_string(), session_id.to_string());
            attrs.insert("gen_ai.conversation.id".to_string(), session_id.to_string());
        }
        if let Some(run_id) = context
            .workflow_run_id
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            attrs.insert("workflow_run_id".to_string(), run_id.to_string());
        }
        if !context.trace_id.is_empty() {
            attrs.insert("trace_id".to_string(), context.trace_id.clone());
        }
    }
    attrs
}
