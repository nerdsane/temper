use std::collections::BTreeMap;
use std::time::SystemTime;

use opentelemetry::trace::{
    Event, Span as OtelSpan, SpanId, SpanKind, Status, TraceContextExt as _, TraceId, Tracer,
};
use serde_json::Value;
use tracing_opentelemetry::OpenTelemetrySpanExt as _;

use crate::types::WasmInvocationContext;

use super::{GuestSpanEndPayload, GuestSpanEntry, allowed_attributes, key_value_from_json};

pub(super) fn manual_parent_context() -> Option<opentelemetry::Context> {
    let context = tracing::Span::current().context();
    let span_context = context.span().span_context().clone();
    span_context
        .is_valid()
        .then(|| opentelemetry::Context::new().with_remote_span_context(span_context))
}

pub(super) fn tracing_span_ids(span: &tracing::Span) -> (Option<TraceId>, Option<SpanId>) {
    let context = span.context();
    let span_context = context.span().span_context().clone();
    if span_context.is_valid() {
        (Some(span_context.trace_id()), Some(span_context.span_id()))
    } else {
        (None, None)
    }
}

pub(super) fn manual_span_attributes(
    context: &WasmInvocationContext,
    span_id: i64,
    attributes: &BTreeMap<String, Value>,
) -> BTreeMap<String, Value> {
    let mut manual = allowed_attributes(attributes);
    manual.insert("tenant".to_string(), Value::String(context.tenant.clone()));
    manual.insert(
        "entity_type".to_string(),
        Value::String(context.entity_type.clone()),
    );
    manual.insert(
        "entity_id".to_string(),
        Value::String(context.entity_id.clone()),
    );
    manual.insert(
        "trigger_action".to_string(),
        Value::String(context.trigger_action.clone()),
    );
    manual.insert(
        "wasm_module".to_string(),
        Value::String(context.wasm_module.clone().unwrap_or_default()),
    );
    manual.insert(
        "agent_id".to_string(),
        Value::String(context.agent_id.clone().unwrap_or_default()),
    );
    manual.insert(
        "session_id".to_string(),
        Value::String(context.session_id.clone().unwrap_or_default()),
    );
    manual.insert(
        "workflow_root_entity_type".to_string(),
        Value::String(
            context
                .workflow_root_entity_type
                .clone()
                .unwrap_or_default(),
        ),
    );
    manual.insert(
        "workflow_root_entity_id".to_string(),
        Value::String(context.workflow_root_entity_id.clone().unwrap_or_default()),
    );
    manual.insert(
        "workflow_run_id".to_string(),
        Value::String(context.workflow_run_id.clone().unwrap_or_default()),
    );
    manual.insert(
        "observability_event".to_string(),
        Value::String("wasm_guest.span".to_string()),
    );
    manual.insert("wasm_guest_span_id".to_string(), Value::from(span_id));
    manual
}

pub(super) fn merge_end_status_attributes(
    target: &mut BTreeMap<String, Value>,
    payload: &GuestSpanEndPayload,
) {
    let status = payload.status.as_deref().unwrap_or("ok");
    if !status.eq_ignore_ascii_case("error") {
        return;
    }
    let error_message = payload
        .error_message
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or("guest span failed");
    let error_type = payload
        .error_type
        .as_deref()
        .filter(|value| !value.is_empty())
        .unwrap_or("guest_span_error");
    target.insert(
        "error.type".to_string(),
        Value::String(error_type.to_string()),
    );
    target.insert(
        "error.message".to_string(),
        Value::String(error_message.to_string()),
    );
    target.insert(
        "exception.message".to_string(),
        Value::String(error_message.to_string()),
    );
}

pub(super) fn status_from_end_payload(payload: &GuestSpanEndPayload) -> Status {
    let status = payload.status.as_deref().unwrap_or("ok");
    if status.eq_ignore_ascii_case("error") {
        let error_message = payload
            .error_message
            .as_deref()
            .filter(|value| !value.is_empty())
            .unwrap_or("guest span failed");
        Status::error(error_message.to_string())
    } else {
        Status::Ok
    }
}

pub(super) fn export_manual_span(entry: &GuestSpanEntry, status: Status, end_time: SystemTime) {
    let (Some(trace_id), Some(span_id), Some(parent_context)) =
        (entry.trace_id, entry.span_id, entry.parent_context.clone())
    else {
        return;
    };

    let tracer = opentelemetry::global::tracer("temper-wasm-guest");
    let attrs = entry
        .attributes
        .iter()
        .filter_map(|(key, value)| key_value_from_json(key, value))
        .collect::<Vec<_>>();
    let events = entry
        .events
        .iter()
        .map(|event| {
            let attrs = event
                .attributes
                .iter()
                .filter_map(|(key, value)| key_value_from_json(key, value))
                .collect::<Vec<_>>();
            Event::new(event.name.clone(), event.timestamp, attrs, 0)
        })
        .collect::<Vec<_>>();
    let mut span = tracer.build_with_context(
        tracer
            .span_builder(entry.name.clone())
            .with_trace_id(trace_id)
            .with_span_id(span_id)
            .with_kind(span_kind(&entry.kind))
            .with_start_time(entry.start_time)
            .with_end_time(end_time)
            .with_status(status)
            .with_attributes(attrs)
            .with_events(events),
        &parent_context,
    );
    span.end_with_timestamp(end_time);
}

fn span_kind(kind: &str) -> SpanKind {
    match kind.to_ascii_lowercase().as_str() {
        "server" => SpanKind::Server,
        "client" => SpanKind::Client,
        "producer" => SpanKind::Producer,
        "consumer" => SpanKind::Consumer,
        _ => SpanKind::Internal,
    }
}
