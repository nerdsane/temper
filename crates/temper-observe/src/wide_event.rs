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
    attrs.push(KeyValue::new(
        "temper.from_status",
        event.from_status.clone(),
    ));
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
        ];
        for e in &events {
            emit_span(e);
            emit_metrics(e);
        }
    }
}
