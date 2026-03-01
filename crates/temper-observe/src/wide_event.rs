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
use opentelemetry::trace::{Span, Status, Tracer};
use serde::{Deserialize, Serialize};
use temper_runtime::scheduler::{sim_now, sim_uuid};

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

/// Build a WideEvent from an entity actor transition.
#[allow(clippy::too_many_arguments)]
pub fn from_transition(
    entity_type: &str,
    entity_id: &str,
    operation: &str,
    from_status: &str,
    to_status: &str,
    success: bool,
    duration_ns: u64,
    params: &serde_json::Value,
    item_count: usize,
    trace_id: &str,
) -> WideEvent {
    let span_id = sim_uuid().to_string();

    let mut tags = BTreeMap::new();
    tags.insert("entity_type".into(), entity_type.into());
    tags.insert("operation".into(), operation.into());
    tags.insert("status".into(), to_status.into());
    tags.insert("success".into(), success.to_string());

    let mut attributes = BTreeMap::new();
    attributes.insert("entity_id".into(), serde_json::json!(entity_id));
    attributes.insert("from_status".into(), serde_json::json!(from_status));
    attributes.insert("params".into(), params.clone());
    attributes.insert("item_count".into(), serde_json::json!(item_count));

    let mut measurements = BTreeMap::new();
    measurements.insert("transition_count".into(), 1.0);
    measurements.insert("duration_ms".into(), duration_ns as f64 / 1_000_000.0);
    measurements.insert("item_count".into(), item_count as f64);

    WideEvent {
        event_kind: EventKind::Transition,
        entity_type: entity_type.into(),
        entity_id: entity_id.into(),
        operation: operation.into(),
        from_status: from_status.into(),
        to_status: to_status.into(),
        success,
        duration_ns,
        timestamp: sim_now(),
        trace_id: trace_id.into(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}

/// Build a WideEvent from a WASM integration module invocation.
#[allow(clippy::too_many_arguments)]
pub fn from_wasm_invocation(
    module_name: &str,
    trigger_action: &str,
    entity_type: &str,
    entity_id: &str,
    tenant: &str,
    success: bool,
    duration_ns: u64,
    error: Option<&str>,
) -> WideEvent {
    let span_id = sim_uuid().to_string();
    let mut tags = BTreeMap::new();
    tags.insert("module_name".into(), module_name.into());
    tags.insert("trigger_action".into(), trigger_action.into());
    tags.insert("success".into(), success.to_string());
    tags.insert("entity_type".into(), entity_type.into());

    let mut attributes = BTreeMap::new();
    attributes.insert("entity_id".into(), serde_json::json!(entity_id));
    attributes.insert("tenant".into(), serde_json::json!(tenant));
    if let Some(err) = error {
        attributes.insert("error".into(), serde_json::json!(err));
    }

    let mut measurements = BTreeMap::new();
    measurements.insert("invocation_count".into(), 1.0);
    measurements.insert("duration_ms".into(), duration_ns as f64 / 1_000_000.0);

    WideEvent {
        event_kind: EventKind::WasmInvocation,
        entity_type: entity_type.into(),
        entity_id: entity_id.into(),
        operation: trigger_action.into(),
        from_status: String::new(),
        to_status: String::new(),
        success,
        duration_ns,
        timestamp: sim_now(),
        trace_id: String::new(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}

/// Build a WideEvent from a Cedar authorization decision.
#[allow(clippy::too_many_arguments)]
pub fn from_authz_decision(
    action: &str,
    resource_type: &str,
    principal_kind: &str,
    decision: &str,
    duration_ns: u64,
    tenant: &str,
) -> WideEvent {
    let span_id = sim_uuid().to_string();
    let mut tags = BTreeMap::new();
    tags.insert("action".into(), action.into());
    tags.insert("resource_type".into(), resource_type.into());
    tags.insert("decision".into(), decision.into());

    let mut attributes = BTreeMap::new();
    attributes.insert("principal_kind".into(), serde_json::json!(principal_kind));
    attributes.insert("tenant".into(), serde_json::json!(tenant));

    let mut measurements = BTreeMap::new();
    measurements.insert("decision_count".into(), 1.0);
    measurements.insert("duration_ns".into(), duration_ns as f64);

    WideEvent {
        event_kind: EventKind::AuthzDecision,
        entity_type: resource_type.into(),
        entity_id: String::new(),
        operation: action.into(),
        from_status: String::new(),
        to_status: String::new(),
        success: decision == "Allow",
        duration_ns,
        timestamp: sim_now(),
        trace_id: String::new(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}

/// Build a WideEvent from an eventual invariant convergence check.
#[allow(clippy::too_many_arguments)]
pub fn from_invariant_check(
    invariant_name: &str,
    entity_type: &str,
    entity_id: &str,
    tenant: &str,
    check_count: u32,
    outcome: &str,
    duration_ns: u64,
) -> WideEvent {
    let span_id = sim_uuid().to_string();
    let mut tags = BTreeMap::new();
    tags.insert("invariant_name".into(), invariant_name.into());
    tags.insert("entity_type".into(), entity_type.into());
    tags.insert("outcome".into(), outcome.into());

    let mut attributes = BTreeMap::new();
    attributes.insert("entity_id".into(), serde_json::json!(entity_id));
    attributes.insert("tenant".into(), serde_json::json!(tenant));
    attributes.insert("check_count".into(), serde_json::json!(check_count));

    let mut measurements = BTreeMap::new();
    measurements.insert("duration_ms".into(), duration_ns as f64 / 1_000_000.0);
    measurements.insert("check_count".into(), check_count as f64);

    WideEvent {
        event_kind: EventKind::InvariantCheck,
        entity_type: entity_type.into(),
        entity_id: entity_id.into(),
        operation: invariant_name.into(),
        from_status: String::new(),
        to_status: String::new(),
        success: outcome == "converged",
        duration_ns,
        timestamp: sim_now(),
        trace_id: String::new(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}

// =========================================================================
// View Projections → OTEL SDK
// =========================================================================

/// Project to the **Contextual View** (OTEL span).
pub fn emit_span(event: &WideEvent) {
    let tracer = opentelemetry::global::tracer("temper");
    let span_name = match event.event_kind {
        EventKind::Transition => format!("{}.{}", event.entity_type, event.operation),
        EventKind::WasmInvocation => format!("wasm.{}", event.operation),
        EventKind::AuthzDecision => format!("authz.{}", event.operation),
        EventKind::InvariantCheck => format!("invariant.{}", event.operation),
    };

    let mut attrs: Vec<KeyValue> = Vec::new();
    for (k, v) in &event.tags {
        attrs.push(KeyValue::new(k.clone(), v.clone()));
    }
    for (k, v) in &event.attributes {
        attrs.push(KeyValue::new(k.clone(), v.to_string()));
    }
    for (k, v) in &event.measurements {
        attrs.push(KeyValue::new(k.clone(), *v));
    }
    attrs.push(KeyValue::new("temper.trace_id", event.trace_id.clone()));
    attrs.push(KeyValue::new("temper.span_id", event.span_id.clone()));
    attrs.push(KeyValue::new("temper.from_status", event.from_status.clone()));
    attrs.push(KeyValue::new("temper.to_status", event.to_status.clone()));

    let status = if event.success {
        Status::Ok
    } else {
        Status::error(String::new())
    };

    let start_time: SystemTime = event.timestamp.into();
    let end_time = start_time + std::time::Duration::from_nanos(event.duration_ns);

    let mut span = tracer
        .span_builder(span_name)
        .with_start_time(start_time)
        .with_attributes(attrs)
        .start(&tracer);

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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event() -> WideEvent {
        from_transition(
            "Order", "order-123", "SubmitOrder", "Draft", "Submitted",
            true, 5_000_000,
            &serde_json::json!({"ShippingAddressId": "addr-1"}),
            2, "trace-abc",
        )
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
    fn test_emit_span_noop() { emit_span(&sample_event()); }

    #[test]
    fn test_emit_metrics_noop() { emit_metrics(&sample_event()); }

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
        let event = from_transition(
            "Order", "order-456", "SubmitOrder", "Draft", "Draft",
            false, 1_000_000, &serde_json::json!({}), 0, "trace-def",
        );
        assert!(!event.success);
        assert_eq!(event.tags["success"], "false");
    }

    #[test]
    fn test_wasm_invocation_event() {
        let event = from_wasm_invocation(
            "weather_module", "CheckWeather", "Task", "task-1", "tenant-a",
            true, 2_000_000, None,
        );
        assert_eq!(event.event_kind, EventKind::WasmInvocation);
        assert_eq!(event.tags["module_name"], "weather_module");
        assert_eq!(event.tags["success"], "true");
        assert!(!event.tags.contains_key("entity_id"));
        assert_eq!(event.attributes["entity_id"], "task-1");
        assert_eq!(event.measurements["invocation_count"], 1.0);
    }

    #[test]
    fn test_wasm_invocation_with_error() {
        let event = from_wasm_invocation(
            "weather_module", "CheckWeather", "Task", "task-1", "tenant-a",
            false, 3_000_000, Some("module panicked"),
        );
        assert!(!event.success);
        assert_eq!(event.attributes["error"], "module panicked");
    }

    #[test]
    fn test_authz_decision_event() {
        let event = from_authz_decision(
            "SubmitOrder", "Order", "user", "Allow", 500_000, "tenant-b",
        );
        assert_eq!(event.event_kind, EventKind::AuthzDecision);
        assert_eq!(event.tags["decision"], "Allow");
        assert!(event.success);
        assert!(!event.tags.contains_key("principal_kind"));
        assert_eq!(event.attributes["principal_kind"], "user");
    }

    #[test]
    fn test_authz_deny_decision() {
        let event = from_authz_decision(
            "DeleteOrder", "Order", "user", "Deny", 800_000, "tenant-b",
        );
        assert!(!event.success);
    }

    #[test]
    fn test_invariant_check_event() {
        let event = from_invariant_check(
            "order_total_positive", "Order", "order-99", "tenant-c",
            3, "converged", 1_500_000,
        );
        assert_eq!(event.event_kind, EventKind::InvariantCheck);
        assert_eq!(event.tags["outcome"], "converged");
        assert!(event.success);
        assert!(!event.tags.contains_key("entity_id"));
        assert_eq!(event.attributes["entity_id"], "order-99");
    }

    #[test]
    fn test_invariant_check_failed() {
        let event = from_invariant_check(
            "stock_non_negative", "Inventory", "inv-5", "tenant-c",
            10, "failed", 5_000_000,
        );
        assert!(!event.success);
    }

    #[test]
    fn test_emit_span_all_event_kinds() {
        let events = vec![
            sample_event(),
            from_wasm_invocation("m", "a", "T", "id", "t", true, 0, None),
            from_authz_decision("a", "T", "user", "Allow", 0, "t"),
            from_invariant_check("inv", "T", "id", "t", 1, "converged", 0),
        ];
        for e in &events { emit_span(e); emit_metrics(e); }
    }
}
