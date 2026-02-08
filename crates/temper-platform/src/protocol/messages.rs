//! WebSocket protocol messages for the platform.
//!
//! All messages between the web UI and the platform server are typed
//! via [`WsMessage`]. Both dev and prod connections share this envelope;
//! irrelevant variants are simply ignored by each side.

use serde::{Deserialize, Serialize};

/// Bidirectional WebSocket message envelope.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsMessage {
    /// User chat message (sent from UI → server).
    Chat {
        /// The user's message text.
        content: String,
    },

    /// Agent response streamed back to the UI (server → UI).
    AgentResponse {
        /// The agent's response text (may be partial for streaming).
        content: String,
        /// Whether this is the final chunk.
        done: bool,
    },

    /// A spec file was generated or updated (server → UI).
    SpecUpdate {
        /// Which spec type changed.
        spec_type: SpecType,
        /// The full spec content.
        content: String,
        /// Entity name this spec belongs to.
        entity_name: String,
    },

    /// Verification cascade status update (server → UI).
    VerifyStatus {
        /// Which cascade level.
        level: String,
        /// Current status.
        status: VerifyStepStatus,
        /// Human-readable summary.
        summary: String,
    },

    /// Deployment status update (server → UI).
    DeployStatus {
        /// Tenant being deployed.
        tenant: String,
        /// Whether deployment succeeded.
        success: bool,
        /// Human-readable summary.
        summary: String,
    },

    /// Evolution engine event (server → dev UI).
    EvolutionEvent {
        /// What kind of evolution event.
        event_type: String,
        /// Summary of the event.
        summary: String,
        /// Associated record ID.
        record_id: String,
    },

    /// Approval request from the evolution engine (server → dev UI).
    ApprovalRequest {
        /// Unique request ID for correlating the response.
        request_id: String,
        /// What is being requested.
        description: String,
        /// The proposed spec change.
        proposed_spec: String,
        /// Priority score (0.0 to 1.0).
        priority: f64,
    },

    /// Approval response from the developer (dev UI → server).
    ApprovalResponse {
        /// Correlates with the `ApprovalRequest.request_id`.
        request_id: String,
        /// Whether the developer approved.
        approved: bool,
        /// Optional rationale from the developer.
        rationale: Option<String>,
    },

    /// Interview phase update (server → dev UI).
    PhaseUpdate {
        /// Current interview phase name.
        phase: String,
        /// Progress percentage (0-100).
        progress: u8,
    },

    /// Error message (server → UI).
    Error {
        /// Error description.
        message: String,
    },
}

/// Type of specification artifact.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SpecType {
    /// I/O Automaton TOML specification.
    IoaToml,
    /// CSDL XML entity model.
    CsdlXml,
    /// Cedar authorization policies.
    Cedar,
}

/// Status of a verification step.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerifyStepStatus {
    /// Step is queued but not started.
    Pending,
    /// Step is currently running.
    Running,
    /// Step passed.
    Passed,
    /// Step failed.
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ws_message_chat_roundtrip() {
        let msg = WsMessage::Chat {
            content: "describe a task tracker".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"type\":\"chat\""));
        let parsed: WsMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            WsMessage::Chat { content } => assert_eq!(content, "describe a task tracker"),
            _ => panic!("expected Chat"),
        }
    }

    #[test]
    fn test_ws_message_agent_response_roundtrip() {
        let msg = WsMessage::AgentResponse {
            content: "I'll help you design that.".into(),
            done: false,
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: WsMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            WsMessage::AgentResponse { content, done } => {
                assert_eq!(content, "I'll help you design that.");
                assert!(!done);
            }
            _ => panic!("expected AgentResponse"),
        }
    }

    #[test]
    fn test_ws_message_spec_update_roundtrip() {
        let msg = WsMessage::SpecUpdate {
            spec_type: SpecType::IoaToml,
            content: "[automaton]\nname = \"Task\"".into(),
            entity_name: "Task".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains("\"spec_type\":\"ioa_toml\""));
        let parsed: WsMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            WsMessage::SpecUpdate {
                spec_type,
                entity_name,
                ..
            } => {
                assert_eq!(spec_type, SpecType::IoaToml);
                assert_eq!(entity_name, "Task");
            }
            _ => panic!("expected SpecUpdate"),
        }
    }

    #[test]
    fn test_ws_message_verify_status() {
        let msg = WsMessage::VerifyStatus {
            level: "L1 Model Check".into(),
            status: VerifyStepStatus::Running,
            summary: "Exploring state space...".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: WsMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            WsMessage::VerifyStatus { level, status, .. } => {
                assert_eq!(level, "L1 Model Check");
                assert_eq!(status, VerifyStepStatus::Running);
            }
            _ => panic!("expected VerifyStatus"),
        }
    }

    #[test]
    fn test_ws_message_approval_flow() {
        let req = WsMessage::ApprovalRequest {
            request_id: "req-001".into(),
            description: "Add SplitOrder action".into(),
            proposed_spec: "[automaton]...".into(),
            priority: 0.87,
        };
        let json = serde_json::to_string(&req).unwrap();
        let _: WsMessage = serde_json::from_str(&json).unwrap();

        let resp = WsMessage::ApprovalResponse {
            request_id: "req-001".into(),
            approved: true,
            rationale: Some("Looks good".into()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: WsMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            WsMessage::ApprovalResponse {
                request_id,
                approved,
                ..
            } => {
                assert_eq!(request_id, "req-001");
                assert!(approved);
            }
            _ => panic!("expected ApprovalResponse"),
        }
    }

    #[test]
    fn test_ws_message_error() {
        let msg = WsMessage::Error {
            message: "Something went wrong".into(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let parsed: WsMessage = serde_json::from_str(&json).unwrap();
        match parsed {
            WsMessage::Error { message } => assert_eq!(message, "Something went wrong"),
            _ => panic!("expected Error"),
        }
    }
}
