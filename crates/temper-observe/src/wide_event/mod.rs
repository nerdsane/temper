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
//! - **Contextual View (Spans)**: full detail including high-cardinality
//!   attributes → sampled, short retention. Used for debugging, investigation,
//!   trajectory analysis.
//!
//! This separates the **instrumentation model** (what the actor records —
//! everything) from the **storage model** (what the backend keeps —
//! policy-driven), so cost and detail tradeoffs are adjusted at runtime
//! without code changes.
//!
//! ## Why This Matters for Agentic Systems
//!
//! Agents don't write instrumentation code. They write I/O Automaton specs,
//! and the actors emit events automatically. The platform must handle all
//! observability without any agent involvement in deciding metrics vs traces
//! vs logs.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use temper_runtime::scheduler::{sim_now, sim_uuid};

mod agent;
mod otel;
mod runtime;
mod transition;

#[cfg(test)]
mod tests;

pub use agent::{LlmCallInput, ToolCallInput, from_llm_call, from_tool_call};
pub use otel::{emit_metrics, emit_span};
pub use runtime::{
    AuthzDecisionInput, InvariantCheckInput, WasmInvocationInput, from_authz_decision,
    from_invariant_check, from_wasm_invocation,
};
pub use transition::{TransitionInput, from_transition};

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
    /// Tags safe for metric grouping: entity_type, operation, status, success.
    pub tags: BTreeMap<String, String>,
    /// Attributes for debugging: entity_id, params, event details.
    /// NOT included in metric tags — this is the cost decoupling.
    pub attributes: BTreeMap<String, serde_json::Value>,
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

pub(crate) fn duration_ms(duration_ns: u64) -> f64 {
    duration_ns as f64 / 1_000_000.0
}

pub(crate) fn event_timestamp() -> DateTime<Utc> {
    sim_now()
}

pub(crate) fn new_span_id() -> String {
    sim_uuid().to_string()
}
