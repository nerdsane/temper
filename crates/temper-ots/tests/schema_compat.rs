//! Schema-compatibility tests for the JCS trajectory-core OTS extensions.
//!
//! The extension fields (`spec_version`/`harness` on metadata, token IDs,
//! response mask and logprobs on turns, `cause_id` on decisions) are optional
//! and additive. Two properties must hold for every stored row:
//!
//! 1. Rows written before the extension still deserialize (back-compat).
//! 2. A trajectory carrying every extension field round-trips losslessly.

use temper_ots::models::{
    DecisionType, MessageRole, OTSChoice, OTSConsequence, OTSContext, OTSDecision, OTSMessage,
    OTSMessageContent, OTSMetadata, OTSSystemMessage, OTSTrajectory, OTSTurn, OutcomeType,
};

/// A trajectory serialized before the JCS extension fields existed.
const PRE_JCS_TRAJECTORY: &str = include_str!("fixtures/trajectory_pre_jcs.json");

fn fixed_timestamp() -> chrono::DateTime<chrono::Utc> {
    "2026-04-28T10:00:00Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .expect("fixture timestamp parses")
}

#[test]
fn pre_extension_trajectory_still_deserializes() {
    let trajectory: OTSTrajectory =
        serde_json::from_str(PRE_JCS_TRAJECTORY).expect("old-shape trajectory must still parse");

    assert_eq!(trajectory.trajectory_id, "traj-pre-jcs-0001");
    assert_eq!(trajectory.version, "0.1.0");
    assert_eq!(trajectory.metadata.agent_id, "agent-support-1");
    assert_eq!(trajectory.metadata.outcome, OutcomeType::Success);
    assert_eq!(trajectory.turns.len(), 1);
    assert_eq!(trajectory.turns[0].decisions.len(), 1);

    // Every extension field is absent, not defaulted to a wrong value.
    assert!(trajectory.metadata.spec_version.is_none());
    assert!(trajectory.metadata.harness.is_none());
    assert!(trajectory.turns[0].prompt_token_ids.is_none());
    assert!(trajectory.turns[0].completion_token_ids.is_none());
    assert!(trajectory.turns[0].response_mask.is_none());
    assert!(trajectory.turns[0].logprobs.is_none());
    assert!(trajectory.turns[0].decisions[0].cause_id.is_none());
}

#[test]
fn pre_extension_trajectory_reserializes_without_extension_keys() {
    let trajectory: OTSTrajectory =
        serde_json::from_str(PRE_JCS_TRAJECTORY).expect("old-shape trajectory must still parse");
    let reserialized = serde_json::to_string(&trajectory).expect("serialize");

    for absent_key in [
        "spec_version",
        "harness",
        "prompt_token_ids",
        "completion_token_ids",
        "response_mask",
        "logprobs",
        "cause_id",
    ] {
        assert!(
            !reserialized.contains(absent_key),
            "unset extension field '{absent_key}' must not be emitted: {reserialized}"
        );
    }
}

#[test]
fn fully_populated_extension_fields_round_trip() {
    let now = fixed_timestamp();

    let metadata = OTSMetadata::new(
        "Run the governed refund workflow",
        "agent-support-1",
        OutcomeType::Success,
        now,
    )
    .with_spec_version("sha256:9f1c2d")
    .with_harness("temperpaw");

    let decision = OTSDecision::new(
        DecisionType::ToolSelection,
        OTSChoice::new("lookup_invoice").with_confidence(0.95),
        OTSConsequence::success().with_result_summary("Found two charges"),
    )
    .with_decision_id("dec-0001")
    .with_cause_id("toolu_01ABCDEF");

    let turn = OTSTurn::new(1, now)
        .with_span_id("span-0001")
        .with_message(OTSMessage::new(
            MessageRole::Assistant,
            OTSMessageContent::text("Looking up the invoice."),
            now,
        ))
        .with_decision(decision)
        .with_prompt_token_ids(vec![128000, 9906, 1917])
        .with_completion_token_ids(vec![40, 2846, 389, 433])
        .with_response_mask(vec![0, 0, 0, 1, 1, 1, 1])
        .with_logprobs(vec![-0.01, -0.42, -1.75, -0.03]);

    let trajectory = OTSTrajectory::new(metadata)
        .with_trajectory_id("traj-jcs-0001")
        .with_context(OTSContext::new())
        .with_system_message(OTSSystemMessage::new("You are a billing agent.", now))
        .with_turn(turn)
        .with_final_reward(0.95);

    let json = serde_json::to_string(&trajectory).expect("serialize");
    let parsed: OTSTrajectory = serde_json::from_str(&json).expect("deserialize");

    assert_eq!(parsed, trajectory, "round-trip must be lossless");
    assert_eq!(
        parsed.metadata.spec_version.as_deref(),
        Some("sha256:9f1c2d")
    );
    assert_eq!(parsed.metadata.harness.as_deref(), Some("temperpaw"));

    let parsed_turn = &parsed.turns[0];
    assert_eq!(
        parsed_turn.prompt_token_ids.as_deref(),
        Some([128000u32, 9906, 1917].as_ref())
    );
    assert_eq!(
        parsed_turn.completion_token_ids.as_deref(),
        Some([40u32, 2846, 389, 433].as_ref())
    );
    assert_eq!(
        parsed_turn.response_mask.as_deref(),
        Some([0u8, 0, 0, 1, 1, 1, 1].as_ref()),
        "1 marks model tokens, 0 marks tool/injected tokens"
    );
    assert_eq!(
        parsed_turn.logprobs.as_deref(),
        Some([-0.01f64, -0.42, -1.75, -0.03].as_ref())
    );
    assert_eq!(
        parsed_turn.decisions[0].cause_id.as_deref(),
        Some("toolu_01ABCDEF"),
        "cause_id links the decision to the observation it produced"
    );
}

#[test]
fn extension_field_names_match_the_cross_repo_contract() {
    // temper and temperpaw must agree on the wire names verbatim; a rename on
    // either side silently drops data instead of failing loudly.
    let now = fixed_timestamp();
    let metadata = OTSMetadata::new("contract", "agent", OutcomeType::Success, now)
        .with_spec_version("v1")
        .with_harness("claude-code");
    let turn = OTSTurn::new(1, now)
        .with_decision(
            OTSDecision::new(
                DecisionType::ToolSelection,
                OTSChoice::new("search"),
                OTSConsequence::success(),
            )
            .with_cause_id("toolu_1"),
        )
        .with_prompt_token_ids(vec![1])
        .with_completion_token_ids(vec![2])
        .with_response_mask(vec![1])
        .with_logprobs(vec![-0.5]);
    let trajectory = OTSTrajectory::new(metadata).with_turn(turn);

    let json = serde_json::to_value(&trajectory).expect("serialize");
    let metadata_json = &json["metadata"];
    assert_eq!(metadata_json["spec_version"], "v1");
    assert_eq!(metadata_json["harness"], "claude-code");

    let turn_json = &json["turns"][0];
    assert_eq!(turn_json["prompt_token_ids"], serde_json::json!([1]));
    assert_eq!(turn_json["completion_token_ids"], serde_json::json!([2]));
    assert_eq!(turn_json["response_mask"], serde_json::json!([1]));
    assert_eq!(turn_json["logprobs"], serde_json::json!([-0.5]));
    assert_eq!(turn_json["decisions"][0]["cause_id"], "toolu_1");
}
