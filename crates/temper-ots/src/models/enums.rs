//! Core enums for OTS

use serde::{Deserialize, Serialize};

/// Types of decisions an agent can make
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionType {
    /// Selection of which tool to use
    ToolSelection,
    /// Choice of parameters for a tool or action
    ParameterChoice,
    /// Step in reasoning process
    ReasoningStep,
    /// Formulation of response to user
    ResponseFormulation,
}

/// Trajectory outcome types
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutcomeType {
    /// Task completed successfully
    Success,
    /// Task partially completed
    PartialSuccess,
    /// Task failed
    Failure,
}

/// Message roles in a turn
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    /// Message from user
    User,
    /// Message from assistant
    Assistant,
    /// System message
    System,
    /// Tool execution result
    Tool,
}

/// Content types for messages
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentType {
    /// Plain text content
    Text,
    /// Tool call request
    ToolCall,
    /// Tool execution response
    ToolResponse,
    /// Interactive widget
    Widget,
}

/// Types of evaluators for annotations
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluatorType {
    /// Human evaluator
    Human,
    /// Model-based evaluator
    Model,
    /// Heuristic-based evaluator
    Heuristic,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decision_type_serialization() {
        let dt = DecisionType::ToolSelection;
        let json = serde_json::to_string(&dt).unwrap();
        assert_eq!(json, r#""tool_selection""#);

        let parsed: DecisionType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, dt);
    }

    #[test]
    fn test_outcome_type_serialization() {
        let ot = OutcomeType::PartialSuccess;
        let json = serde_json::to_string(&ot).unwrap();
        assert_eq!(json, r#""partial_success""#);

        let parsed: OutcomeType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ot);
    }

    #[test]
    fn test_message_role_serialization() {
        let mr = MessageRole::Assistant;
        let json = serde_json::to_string(&mr).unwrap();
        assert_eq!(json, r#""assistant""#);

        let parsed: MessageRole = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, mr);
    }

    #[test]
    fn test_content_type_serialization() {
        let ct = ContentType::ToolCall;
        let json = serde_json::to_string(&ct).unwrap();
        assert_eq!(json, r#""tool_call""#);

        let parsed: ContentType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, ct);
    }

    #[test]
    fn test_evaluator_type_serialization() {
        let et = EvaluatorType::Model;
        let json = serde_json::to_string(&et).unwrap();
        assert_eq!(json, r#""model""#);

        let parsed: EvaluatorType = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, et);
    }
}
