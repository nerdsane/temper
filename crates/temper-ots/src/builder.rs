//! Incremental trajectory builder
//!
//! Provides a [`TrajectoryBuilder`] that accumulates turns incrementally,
//! suitable for capturing trajectories as they unfold during agent execution.

use crate::models::{
    OTSContext, OTSDecision, OTSMessage, OTSMetadata, OTSSystemMessage, OTSTrajectory, OTSTurn,
};
use chrono::{DateTime, Utc};
use temper_runtime::scheduler::sim_now;

/// Incremental builder for constructing trajectories turn by turn.
///
/// # Example
///
/// ```rust,ignore
/// use temper_ots::{TrajectoryBuilder, OTSMetadata, OutcomeType, OTSMessage, MessageRole, OTSMessageContent};
/// use temper_runtime::scheduler::sim_now;
///
/// let now = sim_now();
/// let metadata = OTSMetadata::new("task", "agent", OutcomeType::Success, now);
/// let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());
///
/// builder.start_turn(now);
/// builder.add_message(OTSMessage::new(MessageRole::User, OTSMessageContent::text("Hello"), now));
/// builder.end_turn(now);
///
/// let trajectory = builder.build();
/// ```
#[derive(Clone)]
pub struct TrajectoryBuilder {
    /// Trajectory metadata
    metadata: OTSMetadata,
    /// Initial context
    context: OTSContext,
    /// Optional system message
    system_message: Option<OTSSystemMessage>,
    /// Completed turns
    turns: Vec<OTSTurn>,
    /// Turn currently being built (if any)
    current_turn: Option<OTSTurn>,
}

impl TrajectoryBuilder {
    /// Create a new builder with required metadata and context.
    pub fn new(metadata: OTSMetadata, context: OTSContext) -> Self {
        Self {
            metadata,
            context,
            system_message: None,
            turns: Vec::new(),
            current_turn: None,
        }
    }

    /// Start a new turn. Panics if a turn is already in progress.
    ///
    /// The turn ID is automatically assigned based on the number of
    /// completed turns.
    pub fn start_turn(&mut self, timestamp: DateTime<Utc>) {
        assert!(
            self.current_turn.is_none(),
            "Cannot start a new turn while one is in progress"
        );
        let turn_id = (self.turns.len() + 1) as i32;
        self.current_turn = Some(OTSTurn::new(turn_id, timestamp));
    }

    /// Add a message to the current turn. Panics if no turn is in progress.
    pub fn add_message(&mut self, message: OTSMessage) {
        let turn = self
            .current_turn
            .as_mut()
            .expect("Cannot add message: no turn in progress");
        turn.messages.push(message);
    }

    /// Add a decision to the current turn. Panics if no turn is in progress.
    pub fn add_decision(&mut self, decision: OTSDecision) {
        let turn = self
            .current_turn
            .as_mut()
            .expect("Cannot add decision: no turn in progress");
        turn.decisions.push(decision);
    }

    /// End the current turn, recording its duration. Panics if no turn is in progress.
    ///
    /// Duration is computed as the difference between `end_time` and the
    /// turn's start timestamp.
    pub fn end_turn(&mut self, end_time: DateTime<Utc>) {
        let mut turn = self
            .current_turn
            .take()
            .expect("Cannot end turn: no turn in progress");
        let duration_ms = (end_time - turn.timestamp).num_milliseconds() as f64;
        turn.duration_ms = Some(duration_ms);
        self.turns.push(turn);
    }

    /// Set the system message for the trajectory.
    pub fn set_system_message(&mut self, system_message: OTSSystemMessage) {
        self.system_message = Some(system_message);
    }

    /// Record the actor spec version (hash or version) this run executed under.
    pub fn set_spec_version(&mut self, spec_version: impl Into<String>) {
        self.metadata.spec_version = Some(spec_version.into());
    }

    /// Record the harness that is driving this run (e.g. "temperpaw").
    pub fn set_harness(&mut self, harness: impl Into<String>) {
        self.metadata.harness = Some(harness.into());
    }

    /// Attach serving-stack token IDs to the current turn.
    ///
    /// Panics if no turn is in progress.
    pub fn set_turn_token_ids(
        &mut self,
        prompt_token_ids: Vec<u32>,
        completion_token_ids: Vec<u32>,
    ) {
        let turn = self
            .current_turn
            .as_mut()
            .expect("Cannot set token ids: no turn in progress");
        turn.prompt_token_ids = Some(prompt_token_ids);
        turn.completion_token_ids = Some(completion_token_ids);
    }

    /// Attach the per-token response mask to the current turn.
    ///
    /// `1` marks a model-generated token, `0` a tool or otherwise injected
    /// token. Panics if no turn is in progress.
    pub fn set_turn_response_mask(&mut self, response_mask: Vec<u8>) {
        let turn = self
            .current_turn
            .as_mut()
            .expect("Cannot set response mask: no turn in progress");
        turn.response_mask = Some(response_mask);
    }

    /// Attach per-token log probabilities to the current turn.
    ///
    /// Panics if no turn is in progress.
    pub fn set_turn_logprobs(&mut self, logprobs: Vec<f64>) {
        let turn = self
            .current_turn
            .as_mut()
            .expect("Cannot set logprobs: no turn in progress");
        turn.logprobs = Some(logprobs);
    }

    /// Build the final trajectory, consuming the builder.
    ///
    /// If a turn is still in progress, it is automatically ended using
    /// `sim_now()` as the end time.
    ///
    /// Build a snapshot of the current trajectory without consuming the builder.
    ///
    /// Useful for mid-session uploads where the session should continue
    /// recording new turns after the upload.
    pub fn snapshot(&self) -> OTSTrajectory {
        let mut metadata = self.metadata.clone();
        let now = sim_now(); // determinism-ok: sim_now is DST-safe
        metadata.timestamp_end = Some(now);
        metadata.duration_ms = Some((now - metadata.timestamp_start).num_milliseconds() as f64);

        let mut turns = self.turns.clone();
        if let Some(ref current) = self.current_turn {
            turns.push(current.clone());
        }

        let mut trajectory = OTSTrajectory::new(metadata);
        trajectory.context = self.context.clone();
        trajectory.system_message = self.system_message.clone();
        trajectory.turns = turns;
        trajectory
    }

    /// Build the final trajectory, consuming the builder.
    ///
    /// If a turn is still in progress, it is automatically ended using
    /// `sim_now()` as the end time.
    ///
    /// The metadata's `timestamp_end` is set to `sim_now()` and `duration_ms`
    /// is computed from the start/end timestamps.
    pub fn build(mut self) -> OTSTrajectory {
        // Auto-close any in-progress turn
        if self.current_turn.is_some() {
            let now = sim_now(); // determinism-ok: sim_now is DST-safe
            self.end_turn(now);
        }

        let now = sim_now(); // determinism-ok: sim_now is DST-safe
        self.metadata.timestamp_end = Some(now);
        self.metadata.duration_ms =
            Some((now - self.metadata.timestamp_start).num_milliseconds() as f64);

        let mut trajectory = OTSTrajectory::new(self.metadata);
        trajectory.context = self.context;
        trajectory.system_message = self.system_message;
        trajectory.turns = self.turns;
        trajectory
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{
        DecisionType, MessageRole, OTSChoice, OTSConsequence, OTSMessageContent, OutcomeType,
    };
    use temper_runtime::scheduler::sim_now;

    #[test]
    fn test_builder_basic_flow() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Test task", "agent_1", OutcomeType::Success, now);
        let context = OTSContext::new();
        let mut builder = TrajectoryBuilder::new(metadata, context);

        builder.start_turn(now);
        builder.add_message(OTSMessage::new(
            MessageRole::User,
            OTSMessageContent::text("Hello"),
            now,
        ));
        builder.add_message(OTSMessage::new(
            MessageRole::Assistant,
            OTSMessageContent::text("Hi there"),
            now,
        ));
        builder.end_turn(now);

        let trajectory = builder.build();
        assert_eq!(trajectory.turns.len(), 1);
        assert_eq!(trajectory.turns[0].messages.len(), 2);
        assert_eq!(trajectory.turns[0].turn_id, 1);
    }

    #[test]
    fn test_builder_multiple_turns() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Multi-turn", "agent_2", OutcomeType::Success, now);
        let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        builder.start_turn(now);
        builder.add_message(OTSMessage::new(
            MessageRole::User,
            OTSMessageContent::text("Turn 1"),
            now,
        ));
        builder.end_turn(now);

        builder.start_turn(now);
        builder.add_message(OTSMessage::new(
            MessageRole::User,
            OTSMessageContent::text("Turn 2"),
            now,
        ));
        builder.end_turn(now);

        let trajectory = builder.build();
        assert_eq!(trajectory.turns.len(), 2);
        assert_eq!(trajectory.turns[0].turn_id, 1);
        assert_eq!(trajectory.turns[1].turn_id, 2);
    }

    #[test]
    fn test_builder_with_decisions() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Decision task", "agent_3", OutcomeType::Success, now);
        let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        builder.start_turn(now);
        let decision = OTSDecision::new(
            DecisionType::ToolSelection,
            OTSChoice::new("search"),
            OTSConsequence::success(),
        );
        builder.add_decision(decision);
        builder.end_turn(now);

        let trajectory = builder.build();
        assert_eq!(trajectory.turns[0].decisions.len(), 1);
    }

    #[test]
    fn test_builder_with_system_message() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Sys msg task", "agent_4", OutcomeType::Success, now);
        let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        builder.set_system_message(OTSSystemMessage::new("You are helpful", now));

        let trajectory = builder.build();
        assert!(trajectory.system_message.is_some());
        assert_eq!(
            trajectory.system_message.unwrap().content,
            "You are helpful"
        );
    }

    #[test]
    fn test_builder_auto_closes_turn() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Auto-close", "agent_5", OutcomeType::Success, now);
        let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        builder.start_turn(now);
        builder.add_message(OTSMessage::new(
            MessageRole::User,
            OTSMessageContent::text("Unclosed turn"),
            now,
        ));

        // Build should auto-close the turn
        let trajectory = builder.build();
        assert_eq!(trajectory.turns.len(), 1);
    }

    #[test]
    fn test_builder_sets_end_timestamp() {
        let now = sim_now();
        let metadata = OTSMetadata::new("End time", "agent_6", OutcomeType::Success, now);
        let builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        let trajectory = builder.build();
        assert!(trajectory.metadata.timestamp_end.is_some());
        assert!(trajectory.metadata.duration_ms.is_some());
    }

    #[test]
    fn test_snapshot_does_not_consume_builder() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Snapshot", "agent-snap", OutcomeType::Success, now);
        let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        builder.start_turn(now);
        builder.add_message(OTSMessage::new(
            MessageRole::User,
            OTSMessageContent::text("in-progress"),
            now,
        ));

        let snapshot = builder.snapshot();
        assert_eq!(
            snapshot.turns.len(),
            1,
            "snapshot should include in-progress turn"
        );

        // Builder should remain usable after snapshot.
        builder.end_turn(now);
        let final_trajectory = builder.build();
        assert_eq!(final_trajectory.turns.len(), 1);
    }

    #[test]
    #[should_panic(expected = "Cannot start a new turn while one is in progress")]
    fn test_builder_double_start_panics() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Double start", "agent_7", OutcomeType::Success, now);
        let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        builder.start_turn(now);
        builder.start_turn(now); // Should panic
    }

    #[test]
    #[should_panic(expected = "Cannot end turn: no turn in progress")]
    fn test_builder_end_without_start_panics() {
        let now = sim_now();
        let metadata = OTSMetadata::new("No start", "agent_8", OutcomeType::Success, now);
        let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        builder.end_turn(now); // Should panic
    }

    #[test]
    #[should_panic(expected = "Cannot add message: no turn in progress")]
    fn test_builder_message_without_turn_panics() {
        let now = sim_now();
        let metadata = OTSMetadata::new("No turn", "agent_9", OutcomeType::Success, now);
        let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        builder.add_message(OTSMessage::new(
            MessageRole::User,
            OTSMessageContent::text("Orphan"),
            now,
        ));
    }

    #[test]
    fn test_builder_records_spec_version_and_harness() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Provenance", "agent-prov", OutcomeType::Success, now);
        let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        builder.set_spec_version("sha256:abc123");
        builder.set_harness("temperpaw");

        let trajectory = builder.build();
        assert_eq!(
            trajectory.metadata.spec_version.as_deref(),
            Some("sha256:abc123")
        );
        assert_eq!(trajectory.metadata.harness.as_deref(), Some("temperpaw"));
    }

    #[test]
    fn test_builder_records_turn_token_signals() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Token signals", "agent-tok", OutcomeType::Success, now);
        let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        builder.start_turn(now);
        builder.set_turn_token_ids(vec![1, 2, 3], vec![4, 5]);
        builder.set_turn_response_mask(vec![1, 1, 0, 1, 1]);
        builder.set_turn_logprobs(vec![-0.1, -0.2, -0.3, -0.4, -0.5]);
        builder.end_turn(now);

        let trajectory = builder.build();
        let turn = &trajectory.turns[0];
        assert_eq!(
            turn.prompt_token_ids.as_deref(),
            Some([1u32, 2, 3].as_ref())
        );
        assert_eq!(
            turn.completion_token_ids.as_deref(),
            Some([4u32, 5].as_ref())
        );
        assert_eq!(
            turn.response_mask.as_deref(),
            Some([1u8, 1, 0, 1, 1].as_ref())
        );
        assert_eq!(turn.logprobs.as_ref().map(Vec::len), Some(5));
    }

    #[test]
    fn test_builder_snapshot_carries_token_signals() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Snapshot tokens", "agent-snap", OutcomeType::Success, now);
        let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        builder.set_harness("claude-code");
        builder.start_turn(now);
        builder.set_turn_token_ids(vec![7, 8], vec![9]);

        let snapshot = builder.snapshot();
        assert_eq!(snapshot.metadata.harness.as_deref(), Some("claude-code"));
        assert_eq!(
            snapshot.turns[0].completion_token_ids.as_deref(),
            Some([9u32].as_ref())
        );
    }

    #[test]
    #[should_panic(expected = "Cannot set token ids: no turn in progress")]
    fn test_builder_token_ids_without_turn_panics() {
        let now = sim_now();
        let metadata = OTSMetadata::new("No turn", "agent-tok-2", OutcomeType::Success, now);
        let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        builder.set_turn_token_ids(vec![1], vec![2]);
    }

    #[test]
    #[should_panic(expected = "Cannot set response mask: no turn in progress")]
    fn test_builder_response_mask_without_turn_panics() {
        let now = sim_now();
        let metadata = OTSMetadata::new("No turn", "agent-tok-3", OutcomeType::Success, now);
        let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        builder.set_turn_response_mask(vec![1]);
    }

    #[test]
    #[should_panic(expected = "Cannot set logprobs: no turn in progress")]
    fn test_builder_logprobs_without_turn_panics() {
        let now = sim_now();
        let metadata = OTSMetadata::new("No turn", "agent-tok-4", OutcomeType::Success, now);
        let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        builder.set_turn_logprobs(vec![-0.1]);
    }

    #[test]
    fn test_builder_empty_trajectory() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Empty", "agent_10", OutcomeType::Failure, now);
        let builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        let trajectory = builder.build();
        assert!(trajectory.turns.is_empty());
        assert_eq!(trajectory.version, "0.1.0");
    }
}
