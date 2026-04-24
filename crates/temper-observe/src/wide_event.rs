//! Telemetry as Views: automatic dual-view projection from actor events.
//!
//! Every entity actor transition already produces an `EntityEvent` containing
//! all context (action, from_status, to_status, params, timestamp). This IS
//! the "wide event." No instrumentation code is needed — not for developers,
//! not for agents.
//!
//! The platform automatically projects each wide event into two views:
//!
//! - **Aggregated View (Metrics)**: operation + low-cardinality tags → precise,
//!   long retention, 100% of data points. Used for monitoring, alerting, SLOs.
//!
//! - **Contextual View (Spans)**: full detail including high-cardinality
//!   attributes → sampled, short retention. Used for debugging, investigation,
//!   trajectory analysis.
//!
//! This separates the **instrumentation model** (what the actor records — everything)
//! from the **storage model** (what the backend keeps — policy-driven), so cost and
//! detail tradeoffs are adjusted at runtime without code changes.
//!
//! ## Why This Matters for Agentic Systems
//!
//! Agents don't write instrumentation code. They write I/O Automaton specs, and the actors
//! emit events automatically. The platform must handle all observability without
//! any agent involvement in deciding metrics vs traces vs logs.

use std::collections::BTreeMap;
use std::time::SystemTime;

use chrono::{DateTime, Utc};
use opentelemetry::KeyValue;
use opentelemetry::trace::{
    Span, SpanContext, SpanId, Status, TraceContextExt, TraceFlags, TraceId, TraceState, Tracer,
};
use serde::{Deserialize, Serialize};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use tracing_opentelemetry::OpenTelemetrySpanExt;

/// Discriminant for the kind of wide event being emitted.
///
/// The existing `emit_span()` / `emit_metrics()` projections work off generic
/// tags/attributes/measurements maps — only span naming needs event-kind awareness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventKind {
    /// Entity state transition (existing behavior).
    Transition,
    /// WASM integration module invocation.
    WasmInvocation,
    /// Cedar authorization decision.
    AuthzDecision,
    /// Eventual invariant convergence check.
    InvariantCheck,
    /// LLM API call (model invocation with gen_ai.* semantic conventions).
    LlmCall,
    /// Agent tool invocation (tool_use block execution).
    ToolCall,
}

/// A wide event: the unified telemetry primitive emitted by entity actors.
///
/// This is NOT constructed by developers or agents. It is automatically
/// derived from every `EntityEvent` produced by the actor runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WideEvent {
    /// The kind of event this represents.
    pub event_kind: EventKind,
    /// Entity type (e.g., "Order").
    pub entity_type: String,
    /// Entity ID.
    pub entity_id: String,
    /// Operation (e.g., "SubmitOrder", "CancelOrder").
    pub operation: String,
    /// Status before the transition.
    pub from_status: String,
    /// Status after the transition.
    pub to_status: String,
    /// Whether the transition succeeded.
    pub success: bool,
    /// Duration of the transition in nanoseconds.
    pub duration_ns: u64,
    /// Timestamp.
    pub timestamp: DateTime<Utc>,
    /// Trace ID for correlation.
    pub trace_id: String,
    /// Span ID.
    pub span_id: String,

    // --- Tags (low-cardinality, included in metrics) ---
    /// Tags safe for metric grouping: entity_type, operation, status, success.
    pub tags: BTreeMap<String, String>,

    // --- Attributes (high-cardinality, contextual view only) ---
    /// Attributes for debugging: entity_id, params, event details.
    /// NOT included in metric tags — this is the cost decoupling.
    pub attributes: BTreeMap<String, serde_json::Value>,

    // --- Measurements (numeric values for aggregation) ---
    /// Measurements: transition_count=1, duration_ms, item_count, etc.
    pub measurements: BTreeMap<String, f64>,
}

/// Classification of a field for view projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldClass {
    /// Low-cardinality: safe for metric tags. Included in both views.
    Tag,
    /// High-cardinality: contextual only. NOT in metrics (avoids bill shock).
    Attribute,
    /// Numeric: aggregated in metrics, raw value in traces.
    Measurement,
}

/// Input for building a transition wide event.
pub struct TransitionInput<'a> {
    /// Entity type (e.g., "Order").
    pub entity_type: &'a str,
    /// Entity ID.
    pub entity_id: &'a str,
    /// Operation name (e.g., "SubmitOrder").
    pub operation: &'a str,
    /// Status before the transition.
    pub from_status: &'a str,
    /// Status after the transition.
    pub to_status: &'a str,
    /// Whether the transition succeeded.
    pub success: bool,
    /// Duration in nanoseconds.
    pub duration_ns: u64,
    /// Action parameters.
    pub params: &'a serde_json::Value,
    /// Number of items affected.
    pub item_count: usize,
    /// Trace ID for correlation.
    pub trace_id: &'a str,
}

/// Build a WideEvent from an entity actor transition.
pub fn from_transition(input: TransitionInput<'_>) -> WideEvent {
    let span_id = sim_uuid().to_string();

    let mut tags = BTreeMap::new();
    tags.insert("entity_type".into(), input.entity_type.into());
    tags.insert("operation".into(), input.operation.into());
    tags.insert("status".into(), input.to_status.into());
    tags.insert("success".into(), input.success.to_string());

    let mut attributes = BTreeMap::new();
    attributes.insert("entity_id".into(), serde_json::json!(input.entity_id));
    attributes.insert("from_status".into(), serde_json::json!(input.from_status));
    attributes.insert("params".into(), input.params.clone());
    attributes.insert("item_count".into(), serde_json::json!(input.item_count));

    let mut measurements = BTreeMap::new();
    measurements.insert("transition_count".into(), 1.0);
    measurements.insert("duration_ms".into(), input.duration_ns as f64 / 1_000_000.0);
    measurements.insert("item_count".into(), input.item_count as f64);

    WideEvent {
        event_kind: EventKind::Transition,
        entity_type: input.entity_type.into(),
        entity_id: input.entity_id.into(),
        operation: input.operation.into(),
        from_status: input.from_status.into(),
        to_status: input.to_status.into(),
        success: input.success,
        duration_ns: input.duration_ns,
        timestamp: sim_now(),
        trace_id: input.trace_id.into(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}

/// Input for building a WASM invocation wide event.
pub struct WasmInvocationInput<'a> {
    /// WASM module name.
    pub module_name: &'a str,
    /// Action that triggered the invocation.
    pub trigger_action: &'a str,
    /// Entity type.
    pub entity_type: &'a str,
    /// Entity ID.
    pub entity_id: &'a str,
    /// Tenant identifier.
    pub tenant: &'a str,
    /// Whether the invocation succeeded.
    pub success: bool,
    /// Duration in nanoseconds.
    pub duration_ns: u64,
    /// Error message, if any.
    pub error: Option<&'a str>,
}

/// Build a WideEvent from a WASM integration module invocation.
pub fn from_wasm_invocation(input: WasmInvocationInput<'_>) -> WideEvent {
    let span_id = sim_uuid().to_string();
    let mut tags = BTreeMap::new();
    tags.insert("module_name".into(), input.module_name.into());
    tags.insert("trigger_action".into(), input.trigger_action.into());
    tags.insert("success".into(), input.success.to_string());
    tags.insert("entity_type".into(), input.entity_type.into());

    let mut attributes = BTreeMap::new();
    attributes.insert("entity_id".into(), serde_json::json!(input.entity_id));
    attributes.insert("tenant".into(), serde_json::json!(input.tenant));
    if let Some(err) = input.error {
        attributes.insert("error".into(), serde_json::json!(err));
        attributes.insert("error.message".into(), serde_json::json!(err));
        attributes.insert(
            "error.type".into(),
            serde_json::json!(classify_error(EventKind::WasmInvocation, err)),
        );
        attributes.insert("exception.message".into(), serde_json::json!(err));
    }

    let mut measurements = BTreeMap::new();
    measurements.insert("invocation_count".into(), 1.0);
    measurements.insert("duration_ms".into(), input.duration_ns as f64 / 1_000_000.0);

    WideEvent {
        event_kind: EventKind::WasmInvocation,
        entity_type: input.entity_type.into(),
        entity_id: input.entity_id.into(),
        operation: input.trigger_action.into(),
        from_status: String::new(),
        to_status: String::new(),
        success: input.success,
        duration_ns: input.duration_ns,
        timestamp: sim_now(),
        trace_id: String::new(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}

/// Input for building an authorization decision wide event.
pub struct AuthzDecisionInput<'a> {
    /// Authorization action.
    pub action: &'a str,
    /// Resource type being authorized.
    pub resource_type: &'a str,
    /// Kind of principal (user, admin, system).
    pub principal_kind: &'a str,
    /// Decision outcome ("Allow" or "Deny").
    pub decision: &'a str,
    /// Duration in nanoseconds.
    pub duration_ns: u64,
    /// Tenant identifier.
    pub tenant: &'a str,
}

/// Build a WideEvent from a Cedar authorization decision.
pub fn from_authz_decision(input: AuthzDecisionInput<'_>) -> WideEvent {
    let span_id = sim_uuid().to_string();
    let mut tags = BTreeMap::new();
    tags.insert("action".into(), input.action.into());
    tags.insert("resource_type".into(), input.resource_type.into());
    tags.insert("decision".into(), input.decision.into());

    let mut attributes = BTreeMap::new();
    attributes.insert(
        "principal_kind".into(),
        serde_json::json!(input.principal_kind),
    );
    attributes.insert("tenant".into(), serde_json::json!(input.tenant));

    let mut measurements = BTreeMap::new();
    measurements.insert("decision_count".into(), 1.0);
    measurements.insert("duration_ns".into(), input.duration_ns as f64);

    WideEvent {
        event_kind: EventKind::AuthzDecision,
        entity_type: input.resource_type.into(),
        entity_id: String::new(),
        operation: input.action.into(),
        from_status: String::new(),
        to_status: String::new(),
        success: input.decision == "Allow",
        duration_ns: input.duration_ns,
        timestamp: sim_now(),
        trace_id: String::new(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}

/// Input for building an invariant check wide event.
pub struct InvariantCheckInput<'a> {
    /// Invariant name.
    pub invariant_name: &'a str,
    /// Entity type.
    pub entity_type: &'a str,
    /// Entity ID.
    pub entity_id: &'a str,
    /// Tenant identifier.
    pub tenant: &'a str,
    /// Number of checks performed.
    pub check_count: u32,
    /// Outcome ("converged" or "failed").
    pub outcome: &'a str,
    /// Duration in nanoseconds.
    pub duration_ns: u64,
}

/// Build a WideEvent from an eventual invariant convergence check.
pub fn from_invariant_check(input: InvariantCheckInput<'_>) -> WideEvent {
    let span_id = sim_uuid().to_string();
    let mut tags = BTreeMap::new();
    tags.insert("invariant_name".into(), input.invariant_name.into());
    tags.insert("entity_type".into(), input.entity_type.into());
    tags.insert("outcome".into(), input.outcome.into());

    let mut attributes = BTreeMap::new();
    attributes.insert("entity_id".into(), serde_json::json!(input.entity_id));
    attributes.insert("tenant".into(), serde_json::json!(input.tenant));
    attributes.insert("check_count".into(), serde_json::json!(input.check_count));

    let mut measurements = BTreeMap::new();
    measurements.insert("duration_ms".into(), input.duration_ns as f64 / 1_000_000.0);
    measurements.insert("check_count".into(), input.check_count as f64);

    WideEvent {
        event_kind: EventKind::InvariantCheck,
        entity_type: input.entity_type.into(),
        entity_id: input.entity_id.into(),
        operation: input.invariant_name.into(),
        from_status: String::new(),
        to_status: String::new(),
        success: input.outcome == "converged",
        duration_ns: input.duration_ns,
        timestamp: sim_now(),
        trace_id: String::new(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}

/// Input for building an LLM call wide event.
pub struct LlmCallInput<'a> {
    /// LLM provider (e.g., "anthropic", "openrouter", "openai").
    pub provider: &'a str,
    /// Model name (e.g., "claude-sonnet-4-6").
    pub model: &'a str,
    /// Operation name (e.g., "chat").
    pub operation: &'a str,
    /// Entity type (typically "Session").
    pub entity_type: &'a str,
    /// Entity ID (session ID).
    pub entity_id: &'a str,
    /// Session/conversation ID for grouping turns.
    pub session_id: &'a str,
    /// Whether the call succeeded.
    pub success: bool,
    /// Duration in nanoseconds.
    pub duration_ns: u64,
    /// Input tokens consumed.
    pub input_tokens: i64,
    /// Output tokens generated.
    pub output_tokens: i64,
    /// Stop reason (e.g., "end_turn", "tool_use").
    pub stop_reason: &'a str,
    /// System instructions passed separately from chat history.
    pub system_instructions: Option<&'a str>,
    /// Input messages serialized as a JSON array string.
    pub input_messages: Option<&'a str>,
    /// Output messages serialized as a JSON array string.
    pub output_messages: Option<&'a str>,
    /// Trace ID for correlation.
    pub trace_id: &'a str,
    /// Error message, if any.
    pub error: Option<&'a str>,
}

/// Build a WideEvent from an LLM API call.
pub fn from_llm_call(input: LlmCallInput<'_>) -> WideEvent {
    let span_id = sim_uuid().to_string();
    let mut tags = BTreeMap::new();
    tags.insert("gen_ai.system".into(), input.provider.into());
    tags.insert("gen_ai.request.model".into(), input.model.into());
    tags.insert("gen_ai.operation.name".into(), input.operation.into());
    tags.insert(
        "gen_ai.response.finish_reasons".into(),
        input.stop_reason.into(),
    );
    tags.insert("success".into(), input.success.to_string());

    let mut attributes = BTreeMap::new();
    attributes.insert("entity_id".into(), serde_json::json!(input.entity_id));
    attributes.insert(
        "gen_ai.conversation.id".into(),
        serde_json::json!(input.session_id),
    );
    if let Some(system_instructions) = input.system_instructions {
        attributes.insert(
            "gen_ai.system_instructions".into(),
            serde_json::json!(system_instructions),
        );
    }
    if let Some(messages) = input.input_messages {
        attributes.insert("gen_ai.input.messages".into(), serde_json::json!(messages));
    }
    if let Some(messages) = input.output_messages {
        attributes.insert("gen_ai.output.messages".into(), serde_json::json!(messages));
    }
    if let Some(err) = input.error {
        attributes.insert("error".into(), serde_json::json!(err));
        attributes.insert("error.message".into(), serde_json::json!(err));
        attributes.insert(
            "error.type".into(),
            serde_json::json!(classify_error(EventKind::LlmCall, err)),
        );
        attributes.insert("exception.message".into(), serde_json::json!(err));
    }

    let mut measurements = BTreeMap::new();
    measurements.insert(
        "gen_ai.usage.input_tokens".into(),
        input.input_tokens as f64,
    );
    measurements.insert(
        "gen_ai.usage.output_tokens".into(),
        input.output_tokens as f64,
    );
    measurements.insert("duration_ms".into(), input.duration_ns as f64 / 1_000_000.0);
    measurements.insert("invocation_count".into(), 1.0);

    WideEvent {
        event_kind: EventKind::LlmCall,
        entity_type: input.entity_type.into(),
        entity_id: input.entity_id.into(),
        operation: input.operation.into(),
        from_status: String::new(),
        to_status: input.stop_reason.into(),
        success: input.success,
        duration_ns: input.duration_ns,
        timestamp: sim_now(),
        trace_id: input.trace_id.into(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}

/// Input for building a tool call wide event.
pub struct ToolCallInput<'a> {
    /// Tool name (e.g., "temper_create", "sandbox_bash").
    pub tool_name: &'a str,
    /// Tool call identifier from the LLM response.
    pub tool_call_id: Option<&'a str>,
    /// Entity type (typically "Session").
    pub entity_type: &'a str,
    /// Entity ID (session ID).
    pub entity_id: &'a str,
    /// Session/conversation ID for grouping.
    pub session_id: &'a str,
    /// Tool arguments serialized as JSON.
    pub tool_arguments: Option<&'a str>,
    /// Tool result content serialized as JSON or text.
    pub tool_result: Option<&'a str>,
    /// Whether the tool call succeeded.
    pub success: bool,
    /// Duration in nanoseconds.
    pub duration_ns: u64,
    /// Trace ID for correlation.
    pub trace_id: &'a str,
    /// Error message, if any.
    pub error: Option<&'a str>,
}

/// Build a WideEvent from an agent tool invocation.
pub fn from_tool_call(input: ToolCallInput<'_>) -> WideEvent {
    let span_id = sim_uuid().to_string();
    let mut tags = BTreeMap::new();
    tags.insert("gen_ai.operation.name".into(), "execute_tool".into());
    tags.insert("gen_ai.tool.name".into(), input.tool_name.into());
    tags.insert("entity_type".into(), input.entity_type.into());
    tags.insert("success".into(), input.success.to_string());

    let mut attributes = BTreeMap::new();
    attributes.insert("entity_id".into(), serde_json::json!(input.entity_id));
    attributes.insert(
        "gen_ai.conversation.id".into(),
        serde_json::json!(input.session_id),
    );
    if let Some(tool_call_id) = input.tool_call_id {
        attributes.insert(
            "gen_ai.tool.call.id".into(),
            serde_json::json!(tool_call_id),
        );
    }
    if let Some(arguments) = input.tool_arguments {
        attributes.insert(
            "gen_ai.tool.call.arguments".into(),
            serde_json::json!(arguments),
        );
    }
    if let Some(result) = input.tool_result {
        attributes.insert("gen_ai.tool.call.result".into(), serde_json::json!(result));
    }
    if let Some(err) = input.error {
        attributes.insert("error".into(), serde_json::json!(err));
        attributes.insert("error.message".into(), serde_json::json!(err));
        attributes.insert(
            "error.type".into(),
            serde_json::json!(classify_error(EventKind::ToolCall, err)),
        );
        attributes.insert("exception.message".into(), serde_json::json!(err));
    }

    let mut measurements = BTreeMap::new();
    measurements.insert("duration_ms".into(), input.duration_ns as f64 / 1_000_000.0);
    measurements.insert("invocation_count".into(), 1.0);

    WideEvent {
        event_kind: EventKind::ToolCall,
        entity_type: input.entity_type.into(),
        entity_id: input.entity_id.into(),
        operation: "execute_tool".into(),
        from_status: String::new(),
        to_status: String::new(),
        success: input.success,
        duration_ns: input.duration_ns,
        timestamp: sim_now(),
        trace_id: input.trace_id.into(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}

// =========================================================================
// View Projections → OTEL SDK
// =========================================================================

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

fn classify_error(kind: EventKind, error: &str) -> &'static str {
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

fn event_error_message(event: &WideEvent) -> Option<String> {
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

fn projects_to_contextual_span(event_kind: EventKind) -> bool {
    !matches!(event_kind, EventKind::WasmInvocation | EventKind::LlmCall)
}

/// Project to the **Contextual View** (OTEL span).
pub fn emit_span(event: &WideEvent) {
    if !projects_to_contextual_span(event.event_kind) {
        return;
    }

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
    let end_time = start_time + std::time::Duration::from_nanos(event.duration_ns);

    let parent_cx =
        remote_parent_context(event).unwrap_or_else(|| tracing::Span::current().context());
    let mut span = tracer
        .span_builder(span_name)
        .with_start_time(start_time)
        .with_attributes(attrs)
        .start_with_context(&tracer, &parent_cx);

    span.set_status(status);
    span.end_with_timestamp(end_time);
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

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{SdkTracerProvider, SpanData, SpanExporter};
    use std::sync::{Arc, Mutex};

    static TRACER_LOCK: Mutex<()> = Mutex::new(());

    #[derive(Clone, Debug, Default)]
    struct TestSpanExporter {
        spans: Arc<Mutex<Vec<SpanData>>>,
    }

    impl TestSpanExporter {
        fn finished_spans(&self) -> Vec<SpanData> {
            self.spans
                .lock()
                .expect("span exporter lock poisoned")
                .clone()
        }
    }

    impl SpanExporter for TestSpanExporter {
        fn export(
            &mut self,
            batch: Vec<SpanData>,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = OTelSdkResult> + Send + 'static>>
        {
            self.spans
                .lock()
                .expect("span exporter lock poisoned")
                .extend(batch);
            Box::pin(std::future::ready(Ok(())))
        }
    }

    fn sample_event() -> WideEvent {
        from_transition(TransitionInput {
            entity_type: "Order",
            entity_id: "order-123",
            operation: "SubmitOrder",
            from_status: "Draft",
            to_status: "Submitted",
            success: true,
            duration_ns: 5_000_000,
            params: &serde_json::json!({"ShippingAddressId": "addr-1"}),
            item_count: 2,
            trace_id: "trace-abc",
        })
    }

    fn captured_span_names(events: &[WideEvent]) -> Vec<String> {
        let _guard = TRACER_LOCK.lock().expect("tracer lock poisoned");
        let exporter = TestSpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_simple_exporter(exporter.clone())
            .build();
        let previous = opentelemetry::global::set_tracer_provider(provider.clone());
        for event in events {
            emit_span(event);
        }
        provider.force_flush().expect("force flush span exporter");
        opentelemetry::global::set_tracer_provider(previous);
        exporter
            .finished_spans()
            .into_iter()
            .map(|span| span.name.into_owned())
            .collect()
    }

    #[test]
    fn test_wide_event_from_transition() {
        let event = sample_event();
        assert_eq!(event.event_kind, EventKind::Transition);
        assert_eq!(event.entity_type, "Order");
        assert_eq!(event.operation, "SubmitOrder");
        assert_eq!(event.tags["entity_type"], "Order");
        assert_eq!(event.tags["success"], "true");
        assert_eq!(event.measurements["transition_count"], 1.0);
        assert_eq!(event.attributes["entity_id"], "order-123");
    }

    #[test]
    fn test_emit_span_noop() {
        emit_span(&sample_event());
    }

    #[test]
    fn test_emit_span_projects_supported_contextual_spans() {
        let names = captured_span_names(&[
            sample_event(),
            from_authz_decision(AuthzDecisionInput {
                action: "SubmitOrder",
                resource_type: "Order",
                principal_kind: "user",
                decision: "Allow",
                duration_ns: 0,
                tenant: "tenant-a",
            }),
            from_invariant_check(InvariantCheckInput {
                invariant_name: "order_total_positive",
                entity_type: "Order",
                entity_id: "order-123",
                tenant: "tenant-a",
                check_count: 1,
                outcome: "converged",
                duration_ns: 0,
            }),
            from_tool_call(ToolCallInput {
                tool_name: "temper_get",
                tool_call_id: None,
                entity_type: "Session",
                entity_id: "sess-1",
                session_id: "sess-1",
                tool_arguments: None,
                tool_result: None,
                success: true,
                duration_ns: 0,
                trace_id: "trace-tool",
                error: None,
            }),
        ]);

        assert_eq!(
            names,
            vec![
                "Order.SubmitOrder",
                "authz.SubmitOrder",
                "invariant.order_total_positive",
                "tool.execute_tool",
            ]
        );
    }

    #[test]
    fn test_emit_span_skips_llm_and_wasm_shadow_spans() {
        let names = captured_span_names(&[
            from_wasm_invocation(WasmInvocationInput {
                module_name: "provider_caller",
                trigger_action: "CallProvider",
                entity_type: "Session",
                entity_id: "sess-1",
                tenant: "tenant-a",
                success: true,
                duration_ns: 0,
                error: None,
            }),
            from_llm_call(LlmCallInput {
                provider: "anthropic",
                model: "claude-sonnet-4-6",
                operation: "chat",
                entity_type: "Session",
                entity_id: "sess-1",
                session_id: "sess-1",
                success: true,
                duration_ns: 0,
                input_tokens: 0,
                output_tokens: 0,
                stop_reason: "end_turn",
                system_instructions: None,
                input_messages: None,
                output_messages: None,
                trace_id: "trace-llm",
                error: None,
            }),
        ]);

        assert!(names.is_empty(), "unexpected shadow spans: {names:?}");
    }

    #[test]
    fn test_emit_metrics_noop() {
        emit_metrics(&sample_event());
    }

    #[test]
    fn test_cost_decoupling() {
        let event = sample_event();
        assert!(!event.tags.contains_key("entity_id"));
        assert!(!event.tags.contains_key("params"));
        assert!(event.attributes.contains_key("entity_id"));
        assert!(event.attributes.contains_key("params"));
    }

    #[test]
    fn test_failed_transition_marks_error() {
        let event = from_transition(TransitionInput {
            entity_type: "Order",
            entity_id: "order-456",
            operation: "SubmitOrder",
            from_status: "Draft",
            to_status: "Draft",
            success: false,
            duration_ns: 1_000_000,
            params: &serde_json::json!({}),
            item_count: 0,
            trace_id: "trace-def",
        });
        assert!(!event.success);
        assert_eq!(event.tags["success"], "false");
    }

    #[test]
    fn test_wasm_invocation_event() {
        let event = from_wasm_invocation(WasmInvocationInput {
            module_name: "weather_module",
            trigger_action: "CheckWeather",
            entity_type: "Task",
            entity_id: "task-1",
            tenant: "tenant-a",
            success: true,
            duration_ns: 2_000_000,
            error: None,
        });
        assert_eq!(event.event_kind, EventKind::WasmInvocation);
        assert_eq!(event.tags["module_name"], "weather_module");
        assert_eq!(event.tags["success"], "true");
        assert!(!event.tags.contains_key("entity_id"));
        assert_eq!(event.attributes["entity_id"], "task-1");
        assert_eq!(event.measurements["invocation_count"], 1.0);
    }

    #[test]
    fn test_wasm_invocation_with_error() {
        let event = from_wasm_invocation(WasmInvocationInput {
            module_name: "weather_module",
            trigger_action: "CheckWeather",
            entity_type: "Task",
            entity_id: "task-1",
            tenant: "tenant-a",
            success: false,
            duration_ns: 3_000_000,
            error: Some("module panicked"),
        });
        assert!(!event.success);
        assert_eq!(event.attributes["error"], "module panicked");
        assert_eq!(event.attributes["error.message"], "module panicked");
        assert_eq!(event.attributes["error.type"], "wasm_invocation_error");
        assert_eq!(
            event_error_message(&event).as_deref(),
            Some("module panicked")
        );
    }

    #[test]
    fn test_authz_decision_event() {
        let event = from_authz_decision(AuthzDecisionInput {
            action: "SubmitOrder",
            resource_type: "Order",
            principal_kind: "user",
            decision: "Allow",
            duration_ns: 500_000,
            tenant: "tenant-b",
        });
        assert_eq!(event.event_kind, EventKind::AuthzDecision);
        assert_eq!(event.tags["decision"], "Allow");
        assert!(event.success);
        assert!(!event.tags.contains_key("principal_kind"));
        assert_eq!(event.attributes["principal_kind"], "user");
    }

    #[test]
    fn test_authz_deny_decision() {
        let event = from_authz_decision(AuthzDecisionInput {
            action: "DeleteOrder",
            resource_type: "Order",
            principal_kind: "user",
            decision: "Deny",
            duration_ns: 800_000,
            tenant: "tenant-b",
        });
        assert!(!event.success);
    }

    #[test]
    fn test_invariant_check_event() {
        let event = from_invariant_check(InvariantCheckInput {
            invariant_name: "order_total_positive",
            entity_type: "Order",
            entity_id: "order-99",
            tenant: "tenant-c",
            check_count: 3,
            outcome: "converged",
            duration_ns: 1_500_000,
        });
        assert_eq!(event.event_kind, EventKind::InvariantCheck);
        assert_eq!(event.tags["outcome"], "converged");
        assert!(event.success);
        assert!(!event.tags.contains_key("entity_id"));
        assert_eq!(event.attributes["entity_id"], "order-99");
    }

    #[test]
    fn test_invariant_check_failed() {
        let event = from_invariant_check(InvariantCheckInput {
            invariant_name: "stock_non_negative",
            entity_type: "Inventory",
            entity_id: "inv-5",
            tenant: "tenant-c",
            check_count: 10,
            outcome: "failed",
            duration_ns: 5_000_000,
        });
        assert!(!event.success);
    }

    #[test]
    fn test_llm_call_event() {
        let event = from_llm_call(LlmCallInput {
            provider: "anthropic",
            model: "claude-sonnet-4-6",
            operation: "chat",
            entity_type: "Session",
            entity_id: "sess-1",
            session_id: "sess-1",
            success: true,
            duration_ns: 3_000_000_000,
            input_tokens: 150,
            output_tokens: 50,
            stop_reason: "end_turn",
            system_instructions: Some(r#"[{"type":"text","content":"be concise"}]"#),
            input_messages: Some(r#"[{"role":"user","content":"hello"}]"#),
            output_messages: Some(r#"[{"role":"assistant","content":"hi"}]"#),
            trace_id: "trace-llm",
            error: None,
        });
        assert_eq!(event.event_kind, EventKind::LlmCall);
        assert_eq!(event.tags["gen_ai.system"], "anthropic");
        assert_eq!(event.tags["gen_ai.request.model"], "claude-sonnet-4-6");
        assert_eq!(event.tags["gen_ai.operation.name"], "chat");
        assert_eq!(event.tags["gen_ai.response.finish_reasons"], "end_turn");
        assert!(event.success);
        assert_eq!(event.measurements["gen_ai.usage.input_tokens"], 150.0);
        assert_eq!(event.measurements["gen_ai.usage.output_tokens"], 50.0);
        assert_eq!(event.attributes["gen_ai.conversation.id"], "sess-1");
        assert_eq!(
            event.attributes["gen_ai.system_instructions"],
            r#"[{"type":"text","content":"be concise"}]"#
        );
        assert_eq!(
            event.attributes["gen_ai.input.messages"],
            r#"[{"role":"user","content":"hello"}]"#
        );
        assert_eq!(
            event.attributes["gen_ai.output.messages"],
            r#"[{"role":"assistant","content":"hi"}]"#
        );
        assert!(!event.tags.contains_key("entity_id"));
    }

    #[test]
    fn test_llm_call_failure() {
        let event = from_llm_call(LlmCallInput {
            provider: "openrouter",
            model: "gpt-4",
            operation: "chat",
            entity_type: "Session",
            entity_id: "sess-2",
            session_id: "sess-2",
            success: false,
            duration_ns: 500_000,
            input_tokens: 100,
            output_tokens: 0,
            stop_reason: "",
            system_instructions: None,
            input_messages: None,
            output_messages: None,
            trace_id: "trace-fail",
            error: Some("rate limit exceeded"),
        });
        assert!(!event.success);
        assert_eq!(event.attributes["error"], "rate limit exceeded");
    }

    #[test]
    fn test_tool_call_event() {
        let event = from_tool_call(ToolCallInput {
            tool_name: "temper_create",
            tool_call_id: Some("toolu_123"),
            entity_type: "Session",
            entity_id: "sess-1",
            session_id: "sess-1",
            tool_arguments: Some(r#"{"entity":"Task"}"#),
            tool_result: Some(r#"{"id":"task-1"}"#),
            success: true,
            duration_ns: 200_000_000,
            trace_id: "trace-tool",
            error: None,
        });
        assert_eq!(event.event_kind, EventKind::ToolCall);
        assert_eq!(event.tags["gen_ai.operation.name"], "execute_tool");
        assert_eq!(event.tags["gen_ai.tool.name"], "temper_create");
        assert!(event.success);
        assert_eq!(event.attributes["gen_ai.conversation.id"], "sess-1");
        assert_eq!(event.attributes["gen_ai.tool.call.id"], "toolu_123");
        assert_eq!(
            event.attributes["gen_ai.tool.call.arguments"],
            r#"{"entity":"Task"}"#
        );
        assert_eq!(
            event.attributes["gen_ai.tool.call.result"],
            r#"{"id":"task-1"}"#
        );
        assert!(!event.tags.contains_key("entity_id"));
    }

    #[test]
    fn test_tool_call_failure() {
        let event = from_tool_call(ToolCallInput {
            tool_name: "sandbox_bash",
            tool_call_id: None,
            entity_type: "Session",
            entity_id: "sess-3",
            session_id: "sess-3",
            tool_arguments: None,
            tool_result: None,
            success: false,
            duration_ns: 1_000_000,
            trace_id: "trace-tool-fail",
            error: Some("sandbox timeout"),
        });
        assert!(!event.success);
        assert_eq!(event.attributes["error"], "sandbox timeout");
    }

    #[test]
    fn test_emit_span_all_event_kinds() {
        let events = vec![
            sample_event(),
            from_wasm_invocation(WasmInvocationInput {
                module_name: "m",
                trigger_action: "a",
                entity_type: "T",
                entity_id: "id",
                tenant: "t",
                success: true,
                duration_ns: 0,
                error: None,
            }),
            from_authz_decision(AuthzDecisionInput {
                action: "a",
                resource_type: "T",
                principal_kind: "user",
                decision: "Allow",
                duration_ns: 0,
                tenant: "t",
            }),
            from_invariant_check(InvariantCheckInput {
                invariant_name: "inv",
                entity_type: "T",
                entity_id: "id",
                tenant: "t",
                check_count: 1,
                outcome: "converged",
                duration_ns: 0,
            }),
            from_llm_call(LlmCallInput {
                provider: "anthropic",
                model: "claude-sonnet-4-6",
                operation: "chat",
                entity_type: "Session",
                entity_id: "s",
                session_id: "s",
                success: true,
                duration_ns: 0,
                input_tokens: 0,
                output_tokens: 0,
                stop_reason: "end_turn",
                system_instructions: None,
                input_messages: None,
                output_messages: None,
                trace_id: "",
                error: None,
            }),
            from_tool_call(ToolCallInput {
                tool_name: "temper_get",
                tool_call_id: None,
                entity_type: "Session",
                entity_id: "s",
                session_id: "s",
                tool_arguments: None,
                tool_result: None,
                success: true,
                duration_ns: 0,
                trace_id: "",
                error: None,
            }),
        ];
        for e in &events {
            emit_span(e);
            emit_metrics(e);
        }
    }
}
