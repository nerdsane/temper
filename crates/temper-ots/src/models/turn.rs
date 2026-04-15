//! Turn models for interaction cycles
//!
//! DST adaptation: `OTSTurn::new()` uses `sim_uuid()` for span ID generation
//! and accepts an explicit `DateTime<Utc>` timestamp.

use crate::models::{OTSDecision, OTSMessage};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use temper_runtime::scheduler::sim_uuid;

/// One LLM interaction cycle
///
/// Contains messages and extracted decisions
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSTurn {
    /// Turn number in sequence
    pub turn_id: i32,

    /// Span ID for tracing
    pub span_id: String,

    /// Parent span ID for nested traces
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_span_id: Option<String>,

    /// When turn started
    pub timestamp: DateTime<Utc>,

    /// Duration in milliseconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,

    /// Whether turn resulted in error
    #[serde(default)]
    pub error: bool,

    /// Reward assigned to this turn
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_reward: Option<f64>,

    /// Messages in this turn
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<OTSMessage>,

    /// Decisions made in this turn
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decisions: Vec<OTSDecision>,
}

impl OTSTurn {
    /// Create a new turn with the given ID and timestamp.
    ///
    /// Uses `sim_uuid()` for deterministic span ID generation in simulation.
    pub fn new(turn_id: i32, timestamp: DateTime<Utc>) -> Self {
        Self {
            turn_id,
            span_id: sim_uuid().to_string(),
            parent_span_id: None,
            timestamp,
            duration_ms: None,
            error: false,
            turn_reward: None,
            messages: Vec::new(),
            decisions: Vec::new(),
        }
    }

    /// Set the span ID
    pub fn with_span_id(mut self, span_id: impl Into<String>) -> Self {
        self.span_id = span_id.into();
        self
    }

    /// Set the parent span ID
    pub fn with_parent_span_id(mut self, parent_span_id: impl Into<String>) -> Self {
        self.parent_span_id = Some(parent_span_id.into());
        self
    }

    /// Set the duration in milliseconds
    pub fn with_duration_ms(mut self, duration_ms: f64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    /// Mark this turn as an error
    pub fn with_error(mut self, error: bool) -> Self {
        self.error = error;
        self
    }

    /// Set the turn reward
    pub fn with_turn_reward(mut self, turn_reward: f64) -> Self {
        self.turn_reward = Some(turn_reward);
        self
    }

    /// Add a message
    pub fn with_message(mut self, message: OTSMessage) -> Self {
        self.messages.push(message);
        self
    }

    /// Add a decision
    pub fn with_decision(mut self, decision: OTSDecision) -> Self {
        self.decisions.push(decision);
        self
    }

    /// Set all messages
    pub fn with_messages(mut self, messages: Vec<OTSMessage>) -> Self {
        self.messages = messages;
        self
    }

    /// Set all decisions
    pub fn with_decisions(mut self, decisions: Vec<OTSDecision>) -> Self {
        self.decisions = decisions;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{DecisionType, MessageRole, OTSChoice, OTSConsequence, OTSMessageContent};
    use temper_runtime::scheduler::sim_now;

    #[test]
    fn test_turn_serialization() {
        let timestamp = sim_now();
        let turn = OTSTurn::new(1, timestamp)
            .with_duration_ms(150.5)
            .with_turn_reward(0.85);

        let json_str = serde_json::to_string(&turn).unwrap();
        let parsed: OTSTurn = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.turn_id, 1);
        assert_eq!(parsed.duration_ms, Some(150.5));
        assert_eq!(parsed.turn_reward, Some(0.85));
        assert!(!parsed.error);
    }

    #[test]
    fn test_turn_with_messages_and_decisions() {
        let timestamp = sim_now();
        let message = OTSMessage::new(
            MessageRole::User,
            OTSMessageContent::text("Hello"),
            timestamp,
        );
        let decision = OTSDecision::new(
            DecisionType::ToolSelection,
            OTSChoice::new("search"),
            OTSConsequence::success(),
        );

        let turn = OTSTurn::new(1, timestamp)
            .with_message(message)
            .with_decision(decision);

        assert_eq!(turn.messages.len(), 1);
        assert_eq!(turn.decisions.len(), 1);

        let json_str = serde_json::to_string(&turn).unwrap();
        let parsed: OTSTurn = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.messages.len(), 1);
        assert_eq!(parsed.decisions.len(), 1);
    }

    #[test]
    fn test_turn_minimal() {
        let timestamp = sim_now();
        let turn = OTSTurn::new(1, timestamp);

        let json_str = serde_json::to_string(&turn).unwrap();

        // Optional fields should not appear
        assert!(!json_str.contains("\"parent_span_id\""));
        assert!(!json_str.contains("\"duration_ms\""));
        assert!(!json_str.contains("\"turn_reward\""));

        // Empty vectors should not appear
        assert!(!json_str.contains("\"messages\""));
        assert!(!json_str.contains("\"decisions\""));

        // Error defaults to false but should appear
        assert!(json_str.contains("\"error\":false"));
    }

    #[test]
    fn test_turn_with_error() {
        let timestamp = sim_now();
        let turn = OTSTurn::new(1, timestamp).with_error(true);

        assert!(turn.error);

        let json_str = serde_json::to_string(&turn).unwrap();
        assert!(json_str.contains("\"error\":true"));
    }

    #[test]
    fn test_turn_with_parent_span() {
        let timestamp = sim_now();
        let turn = OTSTurn::new(1, timestamp).with_parent_span_id("parent-span-123");

        assert_eq!(turn.parent_span_id, Some("parent-span-123".to_string()));
    }
}
