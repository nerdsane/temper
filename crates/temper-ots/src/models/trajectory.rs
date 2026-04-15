//! Trajectory models - top-level container
//!
//! DST adaptations:
//! - `OTSMetadata::new()` accepts `timestamp_start` as a parameter
//! - `OTSSystemMessage::new()` accepts `timestamp` as a parameter
//! - `OTSTrajectory::new()` uses `sim_uuid()` for ID generation

use crate::models::{OTSContext, OTSTurn, OutcomeType};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use temper_runtime::scheduler::sim_uuid;

/// Trajectory metadata
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSMetadata {
    /// Task description
    pub task_description: String,

    /// Domain (e.g., "customer_support", "coding")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub domain: Option<String>,

    /// When trajectory started
    pub timestamp_start: DateTime<Utc>,

    /// When trajectory ended
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_end: Option<DateTime<Utc>>,

    /// Duration in milliseconds
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<f64>,

    /// Agent identifier
    pub agent_id: String,

    /// Agent framework (e.g., "letta", "langchain")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub framework: Option<String>,

    /// Environment (e.g., "production", "staging")
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,

    /// Trajectory outcome
    pub outcome: OutcomeType,

    /// Feedback score (0.0 to 1.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feedback_score: Option<f64>,

    /// Whether trajectory was reviewed by human
    #[serde(default)]
    pub human_reviewed: bool,

    /// Tags for categorization
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,

    /// Parent trajectory ID (for hierarchical traces)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_trajectory_id: Option<String>,
}

impl OTSMetadata {
    /// Create new metadata with required fields.
    ///
    /// Accepts an explicit `timestamp_start` instead of calling `Utc::now()`.
    pub fn new(
        task_description: impl Into<String>,
        agent_id: impl Into<String>,
        outcome: OutcomeType,
        timestamp_start: DateTime<Utc>,
    ) -> Self {
        Self {
            task_description: task_description.into(),
            domain: None,
            timestamp_start,
            timestamp_end: None,
            duration_ms: None,
            agent_id: agent_id.into(),
            framework: None,
            environment: None,
            outcome,
            feedback_score: None,
            human_reviewed: false,
            tags: Vec::new(),
            parent_trajectory_id: None,
        }
    }

    /// Set the domain
    pub fn with_domain(mut self, domain: impl Into<String>) -> Self {
        self.domain = Some(domain.into());
        self
    }

    /// Set the start timestamp
    pub fn with_timestamp_start(mut self, timestamp_start: DateTime<Utc>) -> Self {
        self.timestamp_start = timestamp_start;
        self
    }

    /// Set the end timestamp
    pub fn with_timestamp_end(mut self, timestamp_end: DateTime<Utc>) -> Self {
        self.timestamp_end = Some(timestamp_end);
        self
    }

    /// Set the duration
    pub fn with_duration_ms(mut self, duration_ms: f64) -> Self {
        self.duration_ms = Some(duration_ms);
        self
    }

    /// Set the framework
    pub fn with_framework(mut self, framework: impl Into<String>) -> Self {
        self.framework = Some(framework.into());
        self
    }

    /// Set the environment
    pub fn with_environment(mut self, environment: impl Into<String>) -> Self {
        self.environment = Some(environment.into());
        self
    }

    /// Set the feedback score (must be between 0.0 and 1.0)
    pub fn with_feedback_score(mut self, feedback_score: f64) -> Self {
        assert!(
            (0.0..=1.0).contains(&feedback_score),
            "Feedback score must be between 0.0 and 1.0, got {}",
            feedback_score
        );
        self.feedback_score = Some(feedback_score);
        self
    }

    /// Mark as human reviewed
    pub fn with_human_reviewed(mut self, human_reviewed: bool) -> Self {
        self.human_reviewed = human_reviewed;
        self
    }

    /// Add a tag
    pub fn with_tag(mut self, tag: impl Into<String>) -> Self {
        self.tags.push(tag.into());
        self
    }

    /// Set all tags
    pub fn with_tags(mut self, tags: Vec<String>) -> Self {
        self.tags = tags;
        self
    }

    /// Set parent trajectory ID
    pub fn with_parent_trajectory_id(mut self, parent_trajectory_id: impl Into<String>) -> Self {
        self.parent_trajectory_id = Some(parent_trajectory_id.into());
        self
    }
}

/// System message at trajectory start
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSSystemMessage {
    /// System message content
    pub content: String,

    /// When system message was created
    pub timestamp: DateTime<Utc>,
}

impl OTSSystemMessage {
    /// Create a new system message with an explicit timestamp.
    ///
    /// Accepts a `DateTime<Utc>` instead of calling `Utc::now()`.
    pub fn new(content: impl Into<String>, timestamp: DateTime<Utc>) -> Self {
        Self {
            content: content.into(),
            timestamp,
        }
    }

    /// Set the timestamp
    pub fn with_timestamp(mut self, timestamp: DateTime<Utc>) -> Self {
        self.timestamp = timestamp;
        self
    }
}

/// Open Trajectory Specification (OTS) format
///
/// A complete record of an agent's execution as a decision trace.
/// Enables: display, context learning, simulation, RL training.
///
/// DST adaptation: uses `sim_uuid()` for trajectory ID generation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OTSTrajectory {
    /// Unique trajectory identifier
    pub trajectory_id: String,

    /// OTS version
    pub version: String,

    /// Trajectory metadata
    pub metadata: OTSMetadata,

    /// Initial context
    #[serde(default)]
    pub context: OTSContext,

    /// System message
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_message: Option<OTSSystemMessage>,

    /// Turns in this trajectory
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turns: Vec<OTSTurn>,

    /// Final reward (0.0 to 1.0)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub final_reward: Option<f64>,
}

impl OTSTrajectory {
    /// Create a new trajectory with the given metadata.
    ///
    /// Uses `sim_uuid()` for deterministic ID generation in simulation.
    pub fn new(metadata: OTSMetadata) -> Self {
        Self {
            trajectory_id: sim_uuid().to_string(),
            version: "0.1.0".to_string(),
            metadata,
            context: OTSContext::new(),
            system_message: None,
            turns: Vec::new(),
            final_reward: None,
        }
    }

    /// Set the trajectory ID
    pub fn with_trajectory_id(mut self, trajectory_id: impl Into<String>) -> Self {
        self.trajectory_id = trajectory_id.into();
        self
    }

    /// Set the version
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Set the context
    pub fn with_context(mut self, context: OTSContext) -> Self {
        self.context = context;
        self
    }

    /// Set the system message
    pub fn with_system_message(mut self, system_message: OTSSystemMessage) -> Self {
        self.system_message = Some(system_message);
        self
    }

    /// Add a turn
    pub fn with_turn(mut self, turn: OTSTurn) -> Self {
        self.turns.push(turn);
        self
    }

    /// Set all turns
    pub fn with_turns(mut self, turns: Vec<OTSTurn>) -> Self {
        self.turns = turns;
        self
    }

    /// Set the final reward (must be between 0.0 and 1.0)
    pub fn with_final_reward(mut self, final_reward: f64) -> Self {
        assert!(
            (0.0..=1.0).contains(&final_reward),
            "Final reward must be between 0.0 and 1.0, got {}",
            final_reward
        );
        self.final_reward = Some(final_reward);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_runtime::scheduler::sim_now;

    #[test]
    fn test_metadata_serialization() {
        let now = sim_now();
        let metadata = OTSMetadata::new(
            "Complete user query",
            "agent_123",
            OutcomeType::Success,
            now,
        )
        .with_domain("customer_support")
        .with_framework("langchain")
        .with_tag("high_priority")
        .with_feedback_score(0.9);

        let json_str = serde_json::to_string(&metadata).unwrap();
        let parsed: OTSMetadata = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.task_description, "Complete user query");
        assert_eq!(parsed.agent_id, "agent_123");
        assert_eq!(parsed.outcome, OutcomeType::Success);
        assert_eq!(parsed.domain, Some("customer_support".to_string()));
        assert_eq!(parsed.feedback_score, Some(0.9));
        assert_eq!(parsed.tags.len(), 1);
    }

    #[test]
    #[should_panic(expected = "Feedback score must be between 0.0 and 1.0")]
    fn test_metadata_invalid_feedback_score() {
        let now = sim_now();
        OTSMetadata::new("test", "agent", OutcomeType::Success, now).with_feedback_score(1.5);
    }

    #[test]
    fn test_system_message_serialization() {
        let now = sim_now();
        let msg = OTSSystemMessage::new("You are a helpful assistant", now);

        let json_str = serde_json::to_string(&msg).unwrap();
        let parsed: OTSSystemMessage = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.content, "You are a helpful assistant");
    }

    #[test]
    fn test_trajectory_serialization() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Test task", "agent_1", OutcomeType::Success, now);
        let system_message = OTSSystemMessage::new("System prompt", now);

        let trajectory = OTSTrajectory::new(metadata)
            .with_system_message(system_message)
            .with_final_reward(0.95);

        let json_str = serde_json::to_string(&trajectory).unwrap();
        let parsed: OTSTrajectory = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.version, "0.1.0");
        assert_eq!(parsed.metadata.task_description, "Test task");
        assert!(parsed.system_message.is_some());
        assert_eq!(parsed.final_reward, Some(0.95));
    }

    #[test]
    fn test_trajectory_minimal() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Minimal task", "agent_2", OutcomeType::Failure, now);
        let trajectory = OTSTrajectory::new(metadata);

        let json_str = serde_json::to_string(&trajectory).unwrap();

        // Optional fields should not appear
        assert!(!json_str.contains("\"system_message\""));
        assert!(!json_str.contains("\"final_reward\""));

        // Empty turns should not appear
        assert!(!json_str.contains("\"turns\""));

        // Context should appear as empty object (default)
        assert!(json_str.contains("\"context\":{}"));
    }

    #[test]
    #[should_panic(expected = "Final reward must be between 0.0 and 1.0")]
    fn test_trajectory_invalid_final_reward() {
        let now = sim_now();
        let metadata = OTSMetadata::new("test", "agent", OutcomeType::Success, now);
        OTSTrajectory::new(metadata).with_final_reward(2.0);
    }

    #[test]
    fn test_trajectory_with_turns() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Task with turns", "agent_3", OutcomeType::Success, now);
        let turn1 = OTSTurn::new(1, now);
        let turn2 = OTSTurn::new(2, now);

        let trajectory = OTSTrajectory::new(metadata)
            .with_turn(turn1)
            .with_turn(turn2);

        assert_eq!(trajectory.turns.len(), 2);

        let json_str = serde_json::to_string(&trajectory).unwrap();
        let parsed: OTSTrajectory = serde_json::from_str(&json_str).unwrap();

        assert_eq!(parsed.turns.len(), 2);
        assert_eq!(parsed.turns[0].turn_id, 1);
        assert_eq!(parsed.turns[1].turn_id, 2);
    }

    #[test]
    fn test_metadata_with_parent_trajectory() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Child task", "agent", OutcomeType::Success, now)
            .with_parent_trajectory_id("parent_traj_123");

        assert_eq!(
            metadata.parent_trajectory_id,
            Some("parent_traj_123".to_string())
        );
    }
}
