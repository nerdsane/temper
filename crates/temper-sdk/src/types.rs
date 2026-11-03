//! Core types for the Temper SDK.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Authorization response from `POST /api/authorize`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthzResponse {
    /// Whether the action was allowed by Cedar policy.
    pub allowed: bool,
    /// Human-readable reason for the decision.
    pub reason: Option<String>,
}

/// One record snapshot and the moves to check against it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthzRecord {
    /// Cedar resource type.
    pub resource_type: String,
    /// Canonical Cedar resource ID.
    pub resource_id: String,
    /// Move names, in answer order.
    pub actions: Vec<String>,
    /// Resource attributes from the record read; shared across all moves.
    #[serde(default)]
    pub context: Value,
}

/// Answers for one record, in request order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthzRecordResponse {
    /// Cedar resource type.
    pub resource_type: String,
    /// Canonical Cedar resource ID.
    pub resource_id: String,
    /// Per-move answers, in request order.
    pub actions: Vec<AuthzActionResponse>,
}

/// A move name and its authorization answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthzActionResponse {
    /// Requested move name.
    pub action: String,
    /// Authorization answer; checks never file a decision.
    #[serde(flatten)]
    pub result: AuthzResponse,
}

/// Response to a page of authorization questions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthzBatchResponse {
    /// Record answers, in request order.
    pub records: Vec<AuthzRecordResponse>,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authz_response_minimal() {
        let json = r#"{"allowed":false,"reason":null}"#;
        let resp: AuthzResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.allowed);
        assert!(resp.reason.is_none());
    }
}
