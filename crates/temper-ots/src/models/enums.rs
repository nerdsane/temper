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

impl DecisionType {
    /// Whether a decision of this type names something the agent tried to
    /// invoke, as opposed to something it thought or said.
    ///
    /// `choice.action` means different things per type. On a tool selection or
    /// a parameter choice it is a callable name — a tool, or a governed
    /// action — and the agent attempted to invoke it. On a reasoning step or a
    /// response formulation it is free-form prose describing the thought
    /// ("compare shipping options"), and nothing was invoked.
    ///
    /// Every consumer that reads `choice.action` as an invocation must ask
    /// this first. Treating prose as an invocation fabricates environment
    /// interactions in exported trajectories and reports imaginary actions in
    /// conformance checks.
    pub fn is_invocation(self) -> bool {
        match self {
            Self::ToolSelection | Self::ParameterChoice => true,
            Self::ReasoningStep | Self::ResponseFormulation => false,
        }
    }
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
