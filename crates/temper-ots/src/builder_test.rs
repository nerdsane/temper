//! Tests for [`TrajectoryBuilder`].

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
    // Two completion tokens, so two mask entries and two log probabilities.
    builder.set_turn_token_ids(vec![1, 2, 3], vec![4, 5]);
    builder.set_turn_response_mask(vec![1, 0]);
    builder.set_turn_logprobs(vec![-0.1, -0.2]);
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
    assert_eq!(turn.response_mask.as_deref(), Some([1u8, 0].as_ref()));
    assert_eq!(turn.logprobs.as_ref().map(Vec::len), Some(2));
}

#[test]
#[should_panic(expected = "no completion_token_ids to align it against")]
fn test_builder_end_turn_rejects_a_mask_with_no_completion_tokens() {
    let now = sim_now();
    let metadata = OTSMetadata::new("Dangling mask", "agent-tok", OutcomeType::Success, now);
    let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

    builder.start_turn(now);
    builder.set_turn_response_mask(vec![1, 0]);
    builder.end_turn(now);
}

#[test]
#[should_panic(expected = "completion-side signals are aligned position for position")]
fn test_builder_rejects_a_misaligned_mask() {
    let now = sim_now();
    let metadata = OTSMetadata::new("Misaligned", "agent-tok", OutcomeType::Success, now);
    let mut builder = TrajectoryBuilder::new(metadata, OTSContext::new());

    builder.start_turn(now);
    builder.set_turn_token_ids(vec![1, 2, 3], vec![4, 5]);
    builder.set_turn_response_mask(vec![1, 0, 1]);
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
