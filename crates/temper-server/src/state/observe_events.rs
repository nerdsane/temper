//! Observe and design-time events broadcast from ServerState.

/// An agent progress event for remote observation via SSE.
///
/// These events are broadcast so that the executor (or any observer) can
/// track agent activity in real time without polling.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AgentProgressEvent {
    /// Tenant that owns the related entity.
    pub tenant: String,
    /// Entity type that emitted the event.
    pub entity_type: String,
    /// Entity ID that emitted the event.
    pub entity_id: String,
    /// Monotonic per-entity event sequence.
    pub seq: u64,
    /// Event kind: "tool_call_started", "tool_call_completed",
    /// "task_started", "task_completed", "agent_completed".
    pub kind: String,
    /// The agent ID this event relates to.
    pub agent_id: String,
    /// Optional tool call ID (for tool_call_* events).
    pub tool_call_id: Option<String>,
    /// Optional tool name (for tool_call_* events).
    pub tool_name: Option<String>,
    /// Optional task ID (for task_* events).
    pub task_id: Option<String>,
    /// Optional result or status message.
    pub message: Option<String>,
    /// ISO-8601 timestamp when the event was created.
    pub timestamp: String,
    /// Optional structured payload.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

/// Unified replayable event stream for a single entity.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EntityObserveEvent {
    /// Tenant that owns the entity.
    pub tenant: String,
    /// Entity type for this event.
    pub entity_type: String,
    /// Entity instance ID.
    pub entity_id: String,
    /// Monotonic per-entity event sequence.
    pub seq: u64,
    /// SSE event name.
    pub event_name: String,
    /// Structured event payload.
    pub data: serde_json::Value,
}

/// Lightweight hint broadcast for the Observe UI SSE refresh stream.
///
/// Each variant signals that a particular domain's data has changed.
/// The frontend subscribes to `/observe/refresh/stream` and re-fetches
/// the relevant REST endpoint when it receives a matching hint.
#[derive(Clone, Debug, serde::Serialize)]
pub enum ObserveRefreshHint {
    Specs,
    Entities,
    Verification,
    Trajectories,
    Agents,
    Policies,
    EvolutionRecords,
    EvolutionInsights,
    UnmetIntents,
    FeatureRequests,
    OsApps,
    Decisions,
}

/// A design-time event emitted during spec loading and verification.
///
/// These events are broadcast via SSE so the observe UI can show
/// verification progress in real time (design-time observation).
#[derive(Debug, Clone, serde::Serialize)]
pub struct DesignTimeEvent {
    /// Event kind: "spec_loaded", "verify_started", "verify_level", "verify_done".
    pub kind: String,
    /// Entity type this event relates to.
    pub entity_type: String,
    /// Tenant this event relates to.
    pub tenant: String,
    /// Human-readable summary.
    pub summary: String,
    /// Verification level name (for "verify_level" events).
    pub level: Option<String>,
    /// Whether this level/entity passed (for "verify_level" and "verify_done" events).
    pub passed: Option<bool>,
    /// ISO-8601 timestamp when the event was created.
    pub timestamp: String,
    /// Step number in the workflow (1=loaded, 2=verify_started, 3-6=L0-L3, 7=done).
    pub step_number: Option<u8>,
    /// Total steps in the workflow (always 7 for verification).
    pub total_steps: Option<u8>,
}
