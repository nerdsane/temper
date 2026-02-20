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

/// A wide event: the unified telemetry primitive emitted by entity actors.
///
/// This is NOT constructed by developers or agents. It is automatically
/// derived from every `EntityEvent` produced by the actor runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WideEvent {
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
///
/// This is the core of "Telemetry as Views" — the same data is classified
/// differently for metrics vs traces, and the classification can be changed
/// at runtime without code changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldClass {
    /// Low-cardinality: safe for metric tags. Included in both views.
    Tag,
    /// High-cardinality: contextual only. NOT in metrics (avoids bill shock).
    /// Can be "promoted" to Tag at runtime if the operator decides the cost is worth it.
    Attribute,
    /// Numeric: aggregated in metrics, raw value in traces.
    Measurement,
}

/// Build a WideEvent from an entity actor transition.
/// This is called automatically by the actor — no agent or developer involvement.
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

    // Tags: low-cardinality, safe for metric grouping
    let mut tags = BTreeMap::new();
    tags.insert("entity_type".into(), entity_type.into());
    tags.insert("operation".into(), operation.into());
    tags.insert("status".into(), to_status.into());
    tags.insert("success".into(), success.to_string());

    // Attributes: high-cardinality, contextual only
    let mut attributes = BTreeMap::new();
    attributes.insert("entity_id".into(), serde_json::json!(entity_id));
    attributes.insert("from_status".into(), serde_json::json!(from_status));
    attributes.insert("params".into(), params.clone());
    attributes.insert("item_count".into(), serde_json::json!(item_count));

    // Measurements: numeric values for aggregation
    let mut measurements = BTreeMap::new();
    measurements.insert("transition_count".into(), 1.0);
    measurements.insert("duration_ms".into(), duration_ns as f64 / 1_000_000.0);
    measurements.insert("item_count".into(), item_count as f64);

    WideEvent {
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

// =========================================================================
// View Projections → OTEL SDK
// =========================================================================

/// Project to the **Contextual View** (OTEL span).
///
/// Includes EVERYTHING: measurements, tags, attributes.
/// If OTEL is not initialised the global no-op tracer silently discards.
///
/// The span's start and end timestamps are derived from the WideEvent's
/// `timestamp` and `duration_ns`, so the OTEL span duration reflects the
/// actual transition time (evaluate + persist), not the span-creation overhead.
pub fn emit_span(event: &WideEvent) {
    let tracer = opentelemetry::global::tracer("temper");
    let span_name = format!("{}.{}", event.entity_type, event.operation);

    // Build attributes: tags + high-cardinality attrs + measurements
    let mut attrs: Vec<KeyValue> = Vec::new();

    // Tags (low-cardinality)
    for (k, v) in &event.tags {
        attrs.push(KeyValue::new(k.clone(), v.clone()));
    }

    // Attributes (high-cardinality contextual data)
    for (k, v) in &event.attributes {
        attrs.push(KeyValue::new(k.clone(), v.to_string()));
    }

    // Measurements as span attributes (raw values)
    for (k, v) in &event.measurements {
        attrs.push(KeyValue::new(k.clone(), *v));
    }

    // Correlation IDs
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

    // Set explicit start time from the WideEvent timestamp so the span
    // duration reflects the actual transition time, not span-creation overhead.
    // In DST: sim_now() returns logical time, duration_ns is 0 → zero-width span.
    // In production: real wall-clock start, real measured duration.
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
///
/// Records measurements via counters/histograms. Only low-cardinality
/// tags are used as metric attributes — high-cardinality attributes are
/// excluded to avoid bill shock.
///
/// If OTEL is not initialised the global no-op meter silently discards.
pub fn emit_metrics(event: &WideEvent) {
    let meter = opentelemetry::global::meter("temper");

    // Low-cardinality tag attributes only (the cost decoupling)
    let tag_attrs: Vec<KeyValue> = event
        .tags
        .iter()
        .map(|(k, v)| KeyValue::new(k.clone(), v.clone()))
        .collect();

    // Each measurement becomes a separate metric data point
    for (name, value) in &event.measurements {
        let metric_name = format!("temper.{}.{}", event.operation, name);

        // Use a gauge-style recording via histogram (supports arbitrary values)
        let histogram = meter.f64_histogram(metric_name).build();
        histogram.record(*value, &tag_attrs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event() -> WideEvent {
        from_transition(
            "Order",
            "order-123",
            "SubmitOrder",
            "Draft",
            "Submitted",
            true,
            5_000_000,
            &serde_json::json!({"ShippingAddressId": "addr-1"}),
            2,
            "trace-abc",
        )
    }

    #[test]
    fn test_wide_event_from_transition() {
        let event = sample_event();

        assert_eq!(event.entity_type, "Order");
        assert_eq!(event.operation, "SubmitOrder");
        assert_eq!(event.tags["entity_type"], "Order");
        assert_eq!(event.tags["operation"], "SubmitOrder");
        assert_eq!(event.tags["success"], "true");
        assert_eq!(event.measurements["transition_count"], 1.0);
        assert_eq!(event.attributes["entity_id"], "order-123");
    }

    #[test]
    fn test_emit_span_noop() {
        // With no OTEL init, the global no-op tracer silently discards — no panic.
        let event = sample_event();
        emit_span(&event);
    }

    #[test]
    fn test_emit_metrics_noop() {
        // With no OTEL init, the global no-op meter silently discards — no panic.
        let event = sample_event();
        emit_metrics(&event);
    }

    #[test]
    fn test_cost_decoupling() {
        // The key insight: entity_id is high-cardinality (1 per entity).
        // In traditional telemetry, adding it as a metric tag would cause
        // cardinality explosion and bill shock.
        //
        // With Telemetry as Views:
        // - entity_id is an Attribute → NOT in metric tags → zero cost
        // - entity_id IS in the span attributes → full debugging capability
        // - An operator can PROMOTE it to a Tag at runtime if they decide
        //   the cost is worth it for a specific investigation.

        let event = sample_event();

        // Tags (used for metrics) do NOT include high-cardinality fields
        assert!(!event.tags.contains_key("entity_id"));
        assert!(!event.tags.contains_key("params"));

        // Attributes (used for spans) include high-cardinality fields
        assert!(event.attributes.contains_key("entity_id"));
        assert!(event.attributes.contains_key("params"));
    }

    #[test]
    fn test_failed_transition_marks_error() {
        let event = from_transition(
            "Order",
            "order-456",
            "SubmitOrder",
            "Draft",
            "Draft",
            false,
            1_000_000,
            &serde_json::json!({}),
            0,
            "trace-def",
        );

        assert!(!event.success);
        assert_eq!(event.tags["success"], "false");
    }
}
