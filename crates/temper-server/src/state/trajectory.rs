//! Trajectory log types for tracking entity action outcomes.

use std::collections::VecDeque;

/// The source category of a trajectory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TrajectorySource {
    /// Entity action failure (existing behavior).
    Entity,
    /// Platform capability gap (e.g. unknown MCP method).
    Platform,
    /// Authorization denial.
    Authz,
}

/// A single trajectory entry recording the outcome of a dispatched action.
///
/// Captures both successful transitions and failed intents (guard rejection,
/// unknown action, actor timeout) so the Evolution Engine can analyse gaps.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TrajectoryEntry {
    /// ISO-8601 timestamp (DST-safe: uses sim_now()).
    pub timestamp: String,
    /// Tenant that owns the entity.
    pub tenant: String,
    /// Entity type targeted by the action.
    pub entity_type: String,
    /// Entity ID targeted by the action.
    pub entity_id: String,
    /// Action name that was dispatched.
    pub action: String,
    /// Whether the action succeeded.
    pub success: bool,
    /// Entity status before the action (if known).
    pub from_status: Option<String>,
    /// Entity status after the action (if known).
    pub to_status: Option<String>,
    /// Error description for failed intents.
    pub error: Option<String>,
    /// Agent that performed the action (if known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// Session in which the action was performed (if known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Whether this entry represents an authorization denial.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authz_denied: Option<bool>,
    /// Domain or resource that was denied (for WASM authz denials).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied_resource: Option<String>,
    /// WASM module that was denied (for WASM authz denials).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub denied_module: Option<String>,
    /// Source category for this trajectory entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<TrajectorySource>,
    /// Whether this action was governed by a state-machine spec (`true`) or spec-free (`false`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec_governed: Option<bool>,
    /// Agent type classification (e.g. "claude-code", "openclaw").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
}

/// Bounded, append-only trajectory log.
///
/// Uses `VecDeque` with a fixed capacity. When the log is full, the oldest
/// entry is evicted (ring-buffer semantics). Protected by `RwLock` for
/// concurrent access from multiple request handlers.
pub struct TrajectoryLog {
    /// The bounded deque of trajectory entries.
    entries: VecDeque<TrajectoryEntry>,
    /// Maximum capacity.
    capacity: usize,
}

impl TrajectoryLog {
    /// Create a new trajectory log with the given capacity.
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    /// Append an entry, evicting the oldest if at capacity.
    pub fn push(&mut self, entry: TrajectoryEntry) {
        if self.entries.len() >= self.capacity
            && let Some(evicted) = self.entries.pop_front()
        {
            tracing::info!(
                tenant = %evicted.tenant,
                entity_type = %evicted.entity_type,
                entity_id = %evicted.entity_id,
                action = %evicted.action,
                success = evicted.success,
                capacity = self.capacity,
                "trajectory.eviction"
            );
        }
        tracing::debug!(
            tenant = %entry.tenant,
            entity_type = %entry.entity_type,
            entity_id = %entry.entity_id,
            action = %entry.action,
            success = entry.success,
            size_before = self.entries.len(),
            capacity = self.capacity,
            "trajectory.push"
        );
        self.entries.push_back(entry);
    }

    /// Read-only access to all entries (oldest first).
    pub fn entries(&self) -> &VecDeque<TrajectoryEntry> {
        &self.entries
    }
}
