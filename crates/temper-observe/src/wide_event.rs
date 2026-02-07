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
//! Agents don't write instrumentation code. They write TLA+ specs, and the actors
//! emit events automatically. The platform must handle all observability without
//! any agent involvement in deciding metrics vs traces vs logs.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::clickhouse::{LogRecord, MetricRecord, SpanRecord};

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
    pub tags: HashMap<String, String>,

    // --- Attributes (high-cardinality, contextual view only) ---
    /// Attributes for debugging: entity_id, params, event details.
    /// NOT included in metric tags — this is the cost decoupling.
    pub attributes: HashMap<String, serde_json::Value>,

    // --- Measurements (numeric values for aggregation) ---
    /// Measurements: transition_count=1, duration_ms, item_count, etc.
    pub measurements: HashMap<String, f64>,
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
    let span_id = uuid::Uuid::now_v7().to_string();

    // Tags: low-cardinality, safe for metric grouping
    let mut tags = HashMap::new();
    tags.insert("entity_type".into(), entity_type.into());
    tags.insert("operation".into(), operation.into());
    tags.insert("status".into(), to_status.into());
    tags.insert("success".into(), success.to_string());

    // Attributes: high-cardinality, contextual only
    let mut attributes = HashMap::new();
    attributes.insert("entity_id".into(), serde_json::json!(entity_id));
    attributes.insert("from_status".into(), serde_json::json!(from_status));
    attributes.insert("params".into(), params.clone());
    attributes.insert("item_count".into(), serde_json::json!(item_count));

    // Measurements: numeric values for aggregation
    let mut measurements = HashMap::new();
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
        timestamp: Utc::now(),
        trace_id: trace_id.into(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}

// =========================================================================
// View Projections
// =========================================================================

/// Project to the **Aggregated View** (metrics).
///
/// Extracts measurements + tags only. Attributes are discarded.
/// Each measurement becomes a metric data point with low-cardinality tags.
/// Exemplar trace_id links each point back to the full contextual event.
pub fn project_to_metrics(event: &WideEvent) -> Vec<MetricRecord> {
    let ch_ts = event.timestamp.format("%Y-%m-%d %H:%M:%S").to_string();

    event.measurements.iter().map(|(name, value)| {
        let metric_name = format!("temper.{}.{}", event.operation, name);

        // Tags go into the metric — these are low-cardinality, safe for aggregation
        let mut metric_tags = event.tags.clone();
        // Add exemplar link: the trace_id lets you jump from metric → trace
        metric_tags.insert("exemplar.trace_id".into(), event.trace_id.clone());

        MetricRecord {
            metric_name,
            timestamp: ch_ts.clone(),
            value: *value,
            tags: serde_json::to_string(&metric_tags).unwrap_or_default(),
        }
    }).collect()
}

/// Project to the **Contextual View** (span).
///
/// Includes EVERYTHING: measurements, tags, attributes, messages.
/// This is the full-detail view for debugging, investigation, and trajectory analysis.
pub fn project_to_span(event: &WideEvent) -> SpanRecord {
    let ch_ts = event.timestamp.format("%Y-%m-%d %H:%M:%S").to_string();

    // Merge all context into attributes
    let mut all_attrs = serde_json::Map::new();
    for (k, v) in &event.tags {
        all_attrs.insert(k.clone(), serde_json::json!(v));
    }
    for (k, v) in &event.attributes {
        all_attrs.insert(k.clone(), v.clone());
    }
    for (k, v) in &event.measurements {
        all_attrs.insert(k.clone(), serde_json::json!(v));
    }

    SpanRecord {
        trace_id: event.trace_id.clone(),
        span_id: event.span_id.clone(),
        parent_span_id: None,
        service: "temper".into(),
        operation: format!("{}.{}", event.entity_type, event.operation),
        status: if event.success { "ok" } else { "error" }.into(),
        duration_ns: event.duration_ns,
        start_time: ch_ts.clone(),
        end_time: ch_ts,
        attributes: serde_json::to_string(&all_attrs).unwrap_or_default(),
    }
}

/// Project messages to the **Log View**.
pub fn project_to_log(event: &WideEvent, message: &str, level: &str) -> LogRecord {
    let ch_ts = event.timestamp.format("%Y-%m-%d %H:%M:%S").to_string();

    LogRecord {
        timestamp: ch_ts,
        level: level.into(),
        service: "temper".into(),
        message: message.into(),
        attributes: serde_json::json!({
            "entity_type": event.entity_type,
            "entity_id": event.entity_id,
            "operation": event.operation,
            "trace_id": event.trace_id,
        }).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_event() -> WideEvent {
        from_transition(
            "Order", "order-123", "SubmitOrder",
            "Draft", "Submitted", true, 5_000_000,
            &serde_json::json!({"ShippingAddressId": "addr-1"}),
            2, "trace-abc",
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
    fn test_metrics_exclude_high_cardinality() {
        let event = sample_event();
        let metrics = project_to_metrics(&event);

        assert!(metrics.len() >= 2); // transition_count + duration_ms + item_count

        for m in &metrics {
            let tags: HashMap<String, String> = serde_json::from_str(&m.tags).unwrap();
            // Tags include low-cardinality fields
            assert!(tags.contains_key("entity_type"));
            assert!(tags.contains_key("operation"));
            // Tags do NOT include high-cardinality attributes
            assert!(!tags.contains_key("entity_id"));
            assert!(!tags.contains_key("params"));
            // Exemplar link IS present
            assert!(tags.contains_key("exemplar.trace_id"));
        }
    }

    #[test]
    fn test_span_includes_everything() {
        let event = sample_event();
        let span = project_to_span(&event);

        assert_eq!(span.trace_id, "trace-abc");
        assert_eq!(span.operation, "Order.SubmitOrder");
        assert_eq!(span.status, "ok");

        let attrs: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&span.attributes).unwrap();
        // Span includes both tags AND attributes
        assert_eq!(attrs["entity_type"], "Order");
        assert_eq!(attrs["entity_id"], "order-123");
        assert_eq!(attrs["operation"], "SubmitOrder");
        assert_eq!(attrs["from_status"], "Draft");
        // Span includes measurements as raw values
        assert_eq!(attrs["transition_count"], 1.0);
    }

    #[test]
    fn test_exemplar_links_metric_to_trace() {
        let event = sample_event();
        let metrics = project_to_metrics(&event);

        let tags: HashMap<String, String> = serde_json::from_str(&metrics[0].tags).unwrap();
        assert_eq!(tags["exemplar.trace_id"], "trace-abc");
        // An operator can click this exemplar to jump from the metric graph
        // directly to the trace that produced this specific data point.
    }

    #[test]
    fn test_cost_decoupling() {
        // The key insight: entity_id is high-cardinality (1 per entity).
        // In traditional telemetry, adding it as a metric tag would cause
        // cardinality explosion and bill shock.
        //
        // With Telemetry as Views:
        // - entity_id is an Attribute → NOT in metrics → zero cost
        // - entity_id IS in the span → full debugging capability
        // - An operator can PROMOTE it to a Tag at runtime if they decide
        //   the cost is worth it for a specific investigation.

        let event = sample_event();
        let metrics = project_to_metrics(&event);
        let span = project_to_span(&event);

        // Metric: no entity_id
        let tags: HashMap<String, String> = serde_json::from_str(&metrics[0].tags).unwrap();
        assert!(!tags.contains_key("entity_id"), "entity_id must NOT be a metric tag");

        // Span: has entity_id
        let attrs: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&span.attributes).unwrap();
        assert!(attrs.contains_key("entity_id"), "entity_id must be in span attributes");
    }

    #[test]
    fn test_failed_transition_marks_error() {
        let event = from_transition(
            "Order", "order-456", "SubmitOrder",
            "Draft", "Draft", false, 1_000_000,
            &serde_json::json!({}), 0, "trace-def",
        );

        let span = project_to_span(&event);
        assert_eq!(span.status, "error");

        assert_eq!(event.tags["success"], "false");
    }
}
