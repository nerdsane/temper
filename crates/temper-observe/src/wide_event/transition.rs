use std::collections::BTreeMap;

use crate::wide_event::{EventKind, WideEvent, duration_ms, event_timestamp, new_span_id};

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
    let span_id = new_span_id();

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
    measurements.insert("duration_ms".into(), duration_ms(input.duration_ns));
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
        timestamp: event_timestamp(),
        trace_id: input.trace_id.into(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}
