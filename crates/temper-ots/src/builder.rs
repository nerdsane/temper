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
    fn test_builder_empty_trajectory() {
        let now = sim_now();
        let metadata = OTSMetadata::new("Empty", "agent_10", OutcomeType::Failure, now);
        let builder = TrajectoryBuilder::new(metadata, OTSContext::new());

        let trajectory = builder.build();
        assert!(trajectory.turns.is_empty());
        assert_eq!(trajectory.version, "0.1.0");
    }
}
