use std::collections::BTreeMap;

use crate::wide_event::{
    EventKind, WideEvent, duration_ms, event_timestamp, new_span_id, otel::classify_error,
};

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
    let span_id = new_span_id();
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
    measurements.insert("duration_ms".into(), duration_ms(input.duration_ns));

    WideEvent {
        event_kind: EventKind::WasmInvocation,
        entity_type: input.entity_type.into(),
        entity_id: input.entity_id.into(),
        operation: input.trigger_action.into(),
        from_status: String::new(),
        to_status: String::new(),
        success: input.success,
        duration_ns: input.duration_ns,
        timestamp: event_timestamp(),
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
    let span_id = new_span_id();
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
        timestamp: event_timestamp(),
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
    let span_id = new_span_id();
    let mut tags = BTreeMap::new();
    tags.insert("invariant_name".into(), input.invariant_name.into());
    tags.insert("entity_type".into(), input.entity_type.into());
    tags.insert("outcome".into(), input.outcome.into());

    let mut attributes = BTreeMap::new();
    attributes.insert("entity_id".into(), serde_json::json!(input.entity_id));
    attributes.insert("tenant".into(), serde_json::json!(input.tenant));
    attributes.insert("check_count".into(), serde_json::json!(input.check_count));

    let mut measurements = BTreeMap::new();
    measurements.insert("duration_ms".into(), duration_ms(input.duration_ns));
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
        timestamp: event_timestamp(),
        trace_id: String::new(),
        span_id,
        tags,
        attributes,
        measurements,
    }
}
