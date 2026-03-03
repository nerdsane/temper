//! Core types for the Temper SDK.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Authorization response from `POST /api/authorize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthzResponse {
    /// Whether the action was allowed by Cedar policy.
    pub allowed: bool,
    /// Decision ID for pending/escalated decisions.
    pub decision_id: Option<String>,
    /// Human-readable reason for the decision.
    pub reason: Option<String>,
}

/// Audit trail entry for `POST /api/audit`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    /// The agent or principal that performed the action.
    pub agent_id: String,
    /// The action that was performed.
    pub action: String,
    /// The type of resource acted upon.
    pub resource_type: String,
    /// The ID of the resource acted upon.
    pub resource_id: String,
    /// The outcome of the action (e.g., "success", "denied").
    pub outcome: String,
}

/// A server-sent event representing an entity state change.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityEvent {
    /// The entity type (e.g., "Tasks", "Agents").
    pub entity_type: String,
    /// The entity ID.
    pub entity_id: String,
    /// The action or transition that occurred.
    pub action: String,
    /// The event payload.
    pub data: Value,
}
