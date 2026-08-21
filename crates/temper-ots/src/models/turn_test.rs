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
fn completion_side_signals_must_agree_in_length() {
    let timestamp = sim_now();
    let turn = OTSTurn::new(1, timestamp)
        .with_completion_token_ids(vec![4, 5])
        .with_response_mask(vec![1, 0])
        .with_logprobs(vec![-0.1, -0.2]);
    assert!(turn.validate_token_signals().is_ok());
}

#[test]
#[should_panic(expected = "response_mask has 3 entries but completion_token_ids has 2")]
fn a_mask_longer_than_the_completion_panics() {
    let timestamp = sim_now();
    OTSTurn::new(1, timestamp)
        .with_completion_token_ids(vec![4, 5])
        .with_response_mask(vec![1, 0, 1]);
}

#[test]
#[should_panic(expected = "completion_token_ids has 2 entries but response_mask has 3")]
fn the_alignment_check_does_not_depend_on_setter_order() {
    let timestamp = sim_now();
    OTSTurn::new(1, timestamp)
        .with_response_mask(vec![1, 0, 1])
        .with_completion_token_ids(vec![4, 5]);
}

#[test]
#[should_panic(expected = "response_mask entries must be 0 or 1, got 2")]
fn a_mask_outside_the_zero_one_domain_panics() {
    let timestamp = sim_now();
    OTSTurn::new(1, timestamp).with_response_mask(vec![1, 2]);
}

#[test]
fn a_signal_with_nothing_to_align_against_is_rejected_by_validation() {
    let timestamp = sim_now();
    // The setters cannot catch this: there is no counterpart to compare
    // against, so only the whole-turn check sees it.
    let turn = OTSTurn::new(1, timestamp).with_response_mask(vec![1, 0]);
    let error = turn
        .validate_token_signals()
        .expect_err("a mask with no completion tokens is meaningless");
    assert!(error.contains("no completion_token_ids"), "{error}");
}

#[test]
fn a_turn_with_no_token_signals_validates() {
    let timestamp = sim_now();
    assert!(
        OTSTurn::new(1, timestamp).validate_token_signals().is_ok(),
        "token signals are optional; absent is not misaligned"
    );
}

#[test]
fn deserialization_rejects_a_misaligned_turn() {
    let json = serde_json::json!({
        "turn_id": 1,
        "span_id": "span-1",
        "timestamp": "2026-01-01T00:00:00Z",
        "completion_token_ids": [4, 5],
        "logprobs": [-0.1, -0.2, -0.3],
    });
    let error = serde_json::from_value::<OTSTurn>(json)
        .expect_err("a turn with three logprobs over two tokens is not valid RL data");
    assert!(error.to_string().contains("logprobs"), "{error}");
}

#[test]
fn deserialization_rejects_a_signal_with_nothing_to_align_against() {
    let json = serde_json::json!({
        "turn_id": 1,
        "span_id": "span-1",
        "timestamp": "2026-01-01T00:00:00Z",
        "response_mask": [1, 0],
    });
    let error = serde_json::from_value::<OTSTurn>(json)
        .expect_err("a mask with no completion tokens indexes into nothing");
    assert!(
        error.to_string().contains("no completion_token_ids"),
        "{error}"
    );
}

#[test]
fn deserialization_rejects_a_mask_outside_the_zero_one_domain() {
    let json = serde_json::json!({
        "turn_id": 1,
        "span_id": "span-1",
        "timestamp": "2026-01-01T00:00:00Z",
        "completion_token_ids": [4, 5],
        "response_mask": [1, 7],
    });
    let error = serde_json::from_value::<OTSTurn>(json)
        .expect_err("a mask entry of 7 is not a loss switch");
    assert!(error.to_string().contains("0 or 1"), "{error}");
}

#[test]
fn an_aligned_turn_round_trips_through_serde() {
    let timestamp = sim_now();
    let turn = OTSTurn::new(1, timestamp)
        .with_completion_token_ids(vec![4, 5])
        .with_response_mask(vec![1, 0])
        .with_logprobs(vec![-0.1, -0.2]);
    let json = serde_json::to_string(&turn).expect("serialize");
    let parsed: OTSTurn = serde_json::from_str(&json).expect("aligned turns deserialize");
    assert_eq!(parsed, turn);
}

#[test]
fn test_turn_with_parent_span() {
    let timestamp = sim_now();
    let turn = OTSTurn::new(1, timestamp).with_parent_span_id("parent-span-123");

    assert_eq!(turn.parent_span_id, Some("parent-span-123".to_string()));
}
