//! Trajectory context types for tracking agent execution flows.
//!
//! A *trajectory* is the full sequence of actions an agent takes within a
//! single user turn. These types capture the contextual metadata needed
//! for post-hoc analysis, Evolution Record scoring, and sentinel evaluation.

use serde::{Deserialize, Serialize};

/// Contextual metadata attached to every span within a single agent turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryContext {
    /// The distributed trace ID for this trajectory.
    pub trace_id: String,
    /// The turn number within the conversation (1-based).
    pub turn_number: u32,
    /// The parsed user intent, if extracted.
    pub user_intent: Option<String>,
    /// The prompt template version used for this turn.
    pub prompt_version: Option<String>,
    /// The agent identity that handled this turn.
    pub agent_id: Option<String>,
}

impl TrajectoryContext {
    /// Create a new trajectory context with required fields only.
    pub fn new(trace_id: impl Into<String>, turn_number: u32) -> Self {
        Self {
            trace_id: trace_id.into(),
            turn_number,
            user_intent: None,
            prompt_version: None,
            agent_id: None,
        }
    }

    /// Builder: attach a user intent.
    pub fn with_user_intent(mut self, intent: impl Into<String>) -> Self {
        self.user_intent = Some(intent.into());
        self
    }

    /// Builder: attach a prompt version.
    pub fn with_prompt_version(mut self, version: impl Into<String>) -> Self {
        self.prompt_version = Some(version.into());
        self
    }

    /// Builder: attach an agent ID.
    pub fn with_agent_id(mut self, agent_id: impl Into<String>) -> Self {
        self.agent_id = Some(agent_id.into());
        self
    }
}

/// The outcome of a completed trajectory.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryOutcome {
    /// Terminal state: completed, failed, pivoted, abandoned.
    pub outcome: String,
    /// Optional human or automated feedback score (0.0 to 1.0).
    pub feedback_score: Option<f64>,
    /// Total tokens consumed during this trajectory.
    pub total_tokens: Option<u64>,
    /// Total API calls made during this trajectory.
    pub total_api_calls: Option<u32>,
}

impl TrajectoryOutcome {
    /// Create a new outcome with the given terminal state.
    pub fn new(outcome: impl Into<String>) -> Self {
        Self {
            outcome: outcome.into(),
            feedback_score: None,
            total_tokens: None,
            total_api_calls: None,
        }
    }

    /// Builder: attach a feedback score.
    pub fn with_feedback_score(mut self, score: f64) -> Self {
        self.feedback_score = Some(score);
        self
    }

    /// Builder: attach token count.
    pub fn with_total_tokens(mut self, tokens: u64) -> Self {
        self.total_tokens = Some(tokens);
        self
    }

    /// Builder: attach API call count.
    pub fn with_total_api_calls(mut self, calls: u32) -> Self {
        self.total_api_calls = Some(calls);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trajectory_context_builder() {
        let ctx = TrajectoryContext::new("trace-abc", 1)
            .with_user_intent("book a flight")
            .with_prompt_version("v2.3")
            .with_agent_id("agent-42");

        assert_eq!(ctx.trace_id, "trace-abc");
        assert_eq!(ctx.turn_number, 1);
        assert_eq!(ctx.user_intent.as_deref(), Some("book a flight"));
        assert_eq!(ctx.prompt_version.as_deref(), Some("v2.3"));
        assert_eq!(ctx.agent_id.as_deref(), Some("agent-42"));
    }

    #[test]
    fn test_trajectory_context_minimal() {
        let ctx = TrajectoryContext::new("trace-xyz", 5);
        assert_eq!(ctx.trace_id, "trace-xyz");
        assert_eq!(ctx.turn_number, 5);
        assert!(ctx.user_intent.is_none());
        assert!(ctx.prompt_version.is_none());
        assert!(ctx.agent_id.is_none());
    }

    #[test]
    fn test_trajectory_outcome_builder() {
        let outcome = TrajectoryOutcome::new("completed")
            .with_feedback_score(0.95)
            .with_total_tokens(1500)
            .with_total_api_calls(3);

        assert_eq!(outcome.outcome, "completed");
        assert_eq!(outcome.feedback_score, Some(0.95));
        assert_eq!(outcome.total_tokens, Some(1500));
        assert_eq!(outcome.total_api_calls, Some(3));
    }

    #[test]
    fn test_trajectory_outcome_minimal() {
        let outcome = TrajectoryOutcome::new("failed");
        assert_eq!(outcome.outcome, "failed");
        assert!(outcome.feedback_score.is_none());
        assert!(outcome.total_tokens.is_none());
        assert!(outcome.total_api_calls.is_none());
    }

    #[test]
    fn test_trajectory_context_serialization_roundtrip() {
        let ctx = TrajectoryContext::new("trace-rt", 2)
            .with_user_intent("search products");

        let json = serde_json::to_string(&ctx).unwrap();
        let deserialized: TrajectoryContext = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.trace_id, ctx.trace_id);
        assert_eq!(deserialized.turn_number, ctx.turn_number);
        assert_eq!(deserialized.user_intent, ctx.user_intent);
    }

    #[test]
    fn test_trajectory_outcome_serialization_roundtrip() {
        let outcome = TrajectoryOutcome::new("pivoted")
            .with_feedback_score(0.5)
            .with_total_tokens(800);

        let json = serde_json::to_string(&outcome).unwrap();
        let deserialized: TrajectoryOutcome = serde_json::from_str(&json).unwrap();

        assert_eq!(deserialized.outcome, outcome.outcome);
        assert_eq!(deserialized.feedback_score, outcome.feedback_score);
        assert_eq!(deserialized.total_tokens, outcome.total_tokens);
        assert!(deserialized.total_api_calls.is_none());
    }
}
