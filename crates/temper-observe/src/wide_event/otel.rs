use std::time::{Duration, SystemTime};

use opentelemetry::KeyValue;
use opentelemetry::trace::{
    Span, SpanContext, SpanId, Status, TraceContextExt, TraceFlags, TraceId, TraceState, Tracer,
};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::wide_event::{EventKind, WideEvent};

fn key_value_from_json(key: &str, value: &serde_json::Value) -> KeyValue {
    match value {
        serde_json::Value::String(value) => KeyValue::new(key.to_string(), value.clone()),
        serde_json::Value::Bool(value) => KeyValue::new(key.to_string(), *value),
        serde_json::Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                KeyValue::new(key.to_string(), value)
            } else if let Some(value) = value.as_u64().and_then(|value| i64::try_from(value).ok()) {
                KeyValue::new(key.to_string(), value)
            } else if let Some(value) = value.as_f64() {
                KeyValue::new(key.to_string(), value)
            } else {
                KeyValue::new(key.to_string(), value.to_string())
            }
        }
        serde_json::Value::Null | serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
            KeyValue::new(key.to_string(), value.to_string())
        }
    }
}

pub(crate) fn classify_error(kind: EventKind, error: &str) -> &'static str {
    let normalized = error.to_ascii_lowercase();
    if normalized.contains("timeout") {
        "timeout"
    } else if normalized.contains("fuel") {
        "fuel_exhausted"
    } else if normalized.contains("authorization denied") || normalized.contains("forbidden") {
        "authorization_denied"
    } else if normalized.contains("rate limit") {
        "rate_limit"
    } else if normalized.contains("connection") {
        "connection_error"
    } else {
        match kind {
            EventKind::WasmInvocation => "wasm_invocation_error",
            EventKind::LlmCall => "llm_call_error",
            EventKind::ToolCall => "tool_call_error",
            EventKind::AuthzDecision => "authorization_error",
            EventKind::InvariantCheck => "invariant_error",
            EventKind::Transition => "transition_error",
        }
    }
}

pub(crate) fn event_error_message(event: &WideEvent) -> Option<String> {
    event
        .attributes
        .get("error.message")
        .or_else(|| event.attributes.get("exception.message"))
        .or_else(|| event.attributes.get("error"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

fn event_error_type(event: &WideEvent, error_message: &str) -> String {
    event
        .attributes
        .get("error.type")
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| classify_error(event.event_kind, error_message).to_string())
}

fn remote_parent_context(event: &WideEvent) -> Option<opentelemetry::Context> {
    let trace_id = event
        .attributes
        .get("_otel.parent_trace_id")
        .and_then(serde_json::Value::as_str)?;
    let span_id = event
        .attributes
        .get("_otel.parent_span_id")
        .and_then(serde_json::Value::as_str)?;

    let trace_id = TraceId::from_hex(trace_id).ok()?;
    let span_id = SpanId::from_hex(span_id).ok()?;
    let span_context = SpanContext::new(
        trace_id,
        span_id,
        TraceFlags::SAMPLED,
        true,
        TraceState::default(),
    );

    Some(opentelemetry::Context::new().with_remote_span_context(span_context))
}

/// Project to the **Contextual View** (OTEL span).
pub fn emit_span(event: &WideEvent) {
    let tracer = opentelemetry::global::tracer("temper");
    let span_name = match event.event_kind {
        EventKind::Transition => format!("{}.{}", event.entity_type, event.operation),
        EventKind::WasmInvocation => format!("wasm.{}", event.operation),
        EventKind::AuthzDecision => format!("authz.{}", event.operation),
        EventKind::InvariantCheck => format!("invariant.{}", event.operation),
        EventKind::LlmCall => format!("llm.{}", event.operation),
        EventKind::ToolCall => format!("tool.{}", event.operation),
    };

    let mut attrs: Vec<KeyValue> = Vec::new();
    for (k, v) in &event.tags {
        attrs.push(KeyValue::new(k.clone(), v.clone()));
    }
    for (k, v) in &event.attributes {
        if k.starts_with("_otel.") {
            continue;
        }
        attrs.push(key_value_from_json(k, v));
    }
    for (k, v) in &event.measurements {
        attrs.push(KeyValue::new(k.clone(), *v));
    }

    let llm_apm_only = event.event_kind == EventKind::LlmCall;
    if llm_apm_only {
        attrs.push(KeyValue::new("dd_llmobs_enabled", false));
    }
    attrs.push(KeyValue::new("temper.trace_id", event.trace_id.clone()));
    attrs.push(KeyValue::new("temper.span_id", event.span_id.clone()));
    attrs.push(KeyValue::new(
        "temper.from_status",
        event.from_status.clone(),
    ));
    attrs.push(KeyValue::new("temper.to_status", event.to_status.clone()));

    let status = if event.success {
        Status::Ok
    } else {
        let error_message =
            event_error_message(event).unwrap_or_else(|| "operation failed".to_string());
        let error_type = event_error_type(event, &error_message);
        if !event.attributes.contains_key("error.message") {
            attrs.push(KeyValue::new("error.message", error_message.clone()));
        }
        if !event.attributes.contains_key("exception.message") {
            attrs.push(KeyValue::new("exception.message", error_message.clone()));
        }
        if !event.attributes.contains_key("error.type") {
            attrs.push(KeyValue::new("error.type", error_type));
        }
        Status::error(error_message)
    };

    let start_time: SystemTime = event.timestamp.into();
    let end_time = start_time + Duration::from_nanos(event.duration_ns);

    let parent_cx = if llm_apm_only {
        opentelemetry::Context::new()
    } else {
        remote_parent_context(event).unwrap_or_else(|| tracing::Span::current().context())
    };

    let mut span = tracer
        .span_builder(span_name)
        .with_start_time(start_time)
        .with_attributes(attrs)
        .start_with_context(&tracer, &parent_cx);

    span.set_status(status);
    span.end_with_timestamp(end_time);
}

/// Project to the **Aggregated View** (OTEL metrics).
pub fn emit_metrics(event: &WideEvent) {
    let meter = opentelemetry::global::meter("temper");
    let tag_attrs: Vec<KeyValue> = event
        .tags
        .iter()
        .map(|(k, v)| KeyValue::new(k.clone(), v.clone()))
        .collect();

    for (name, value) in &event.measurements {
        let metric_name = format!("temper.{}.{}", event.operation, name);
        let histogram = meter.f64_histogram(metric_name).build();
        histogram.record(*value, &tag_attrs);
    }
}
