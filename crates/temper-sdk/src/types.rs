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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authz_response_serde_roundtrip() {
        let resp = AuthzResponse {
            allowed: true,
            decision_id: Some("dec-123".into()),
            reason: Some("policy matched".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let back: AuthzResponse = serde_json::from_str(&json).unwrap();
        assert!(back.allowed);
        assert_eq!(back.decision_id.unwrap(), "dec-123");
        assert_eq!(back.reason.unwrap(), "policy matched");
    }

    #[test]
    fn authz_response_minimal() {
        let json = r#"{"allowed":false,"decision_id":null,"reason":null}"#;
        let resp: AuthzResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.allowed);
        assert!(resp.decision_id.is_none());
        assert!(resp.reason.is_none());
    }

    #[test]
    fn audit_entry_serde_roundtrip() {
        let entry = AuditEntry {
            agent_id: "agent-007".into(),
            action: "SubmitOrder".into(),
            resource_type: "Order".into(),
            resource_id: "ORD-1".into(),
            outcome: "success".into(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        let back: AuditEntry = serde_json::from_str(&json).unwrap();
        assert_eq!(back.agent_id, "agent-007");
        assert_eq!(back.action, "SubmitOrder");
        assert_eq!(back.outcome, "success");
    }

    #[test]
    fn entity_event_serde_roundtrip() {
        let event = EntityEvent {
            entity_type: "Tasks".into(),
            entity_id: "T-1".into(),
            action: "Complete".into(),
            data: serde_json::json!({"status": "Done"}),
        };
        let json = serde_json::to_string(&event).unwrap();
        let back: EntityEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.entity_type, "Tasks");
        assert_eq!(back.data["status"], "Done");
    }
}
