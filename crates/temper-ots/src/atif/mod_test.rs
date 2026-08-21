use super::*;
use crate::models::{
    DecisionType, OTSChoice, OTSConsequence, OTSMetadata, OTSSystemMessage, OutcomeType,
};
use temper_runtime::scheduler::sim_now;

#[test]
fn non_object_arguments_are_wrapped_not_dropped() {
    let wrapped = object_arguments(Some(&serde_json::json!("plain string")));
    assert_eq!(wrapped, serde_json::json!({"temper.value": "plain string"}));
    assert_eq!(object_arguments(None), serde_json::json!({}));
}

#[test]
fn decision_without_cause_id_falls_back_to_decision_id() {
    let decision = OTSDecision::new(
        DecisionType::ToolSelection,
        OTSChoice::new("Ship"),
        OTSConsequence::success(),
    )
    .with_decision_id("decision-1");
    assert_eq!(tool_call_id(&decision), "decision-1");
    assert_eq!(
        tool_call_id(&decision.with_cause_id("call-9")),
        "call-9",
        "the provider's tool-call id wins over the synthetic decision id"
    );
}

#[test]
fn user_only_turn_emits_no_agent_step() {
    let now = sim_now();
    let metadata = OTSMetadata::new("task", "agent-1", OutcomeType::Success, now);
    let turn = OTSTurn::new(1, now).with_message(OTSMessage::new(
        MessageRole::User,
        OTSMessageContent::text("hello"),
        now,
    ));
    let trajectory = OTSTrajectory::new(metadata)
        .with_system_message(OTSSystemMessage::new("be helpful", now))
        .with_turn(turn);

    let atif = to_atif(&trajectory, None).expect("the system message alone is a step");
    let sources: Vec<AtifSource> = atif.steps.iter().map(|step| step.source).collect();
    assert_eq!(sources, vec![AtifSource::System, AtifSource::User]);
    assert_eq!(atif.steps[1].step_id, 2);
}

#[test]
fn a_trajectory_with_nothing_in_it_is_not_exportable() {
    let now = sim_now();
    let metadata = OTSMetadata::new("task", "agent-1", OutcomeType::Failure, now);
    let trajectory = OTSTrajectory::new(metadata);
    assert_eq!(
        to_atif(&trajectory, None),
        Err(AtifExportError::NoSteps),
        "ATIF v1.7 requires at least one step; a stepless document is not valid ATIF"
    );
}

#[test]
fn a_reasoning_decision_is_not_exported_as_a_tool_call() {
    let now = sim_now();
    let metadata = OTSMetadata::new("task", "agent-1", OutcomeType::Success, now);
    let reasoning = OTSDecision::new(
        DecisionType::ReasoningStep,
        OTSChoice::new("compare shipping options"),
        OTSConsequence::success(),
    );
    let invocation = OTSDecision::new(
        DecisionType::ToolSelection,
        OTSChoice::new("Ship"),
        OTSConsequence::success(),
    );
    let trajectory = OTSTrajectory::new(metadata).with_turn(
        OTSTurn::new(1, now)
            .with_decision(reasoning)
            .with_decision(invocation),
    );

    let atif = to_atif(&trajectory, None).expect("export");
    let step = &atif.steps[0];
    let called: Vec<&str> = step
        .tool_calls
        .iter()
        .map(|call| call.function_name.as_str())
        .collect();
    assert_eq!(
        called,
        vec!["Ship"],
        "a reasoning step names a thought, not a callable"
    );
    assert_eq!(
        step.observation
            .as_ref()
            .expect("observation")
            .results
            .len(),
        1,
        "no environment result is fabricated for a thought"
    );
    assert_eq!(
        step.extra["temper.decisions"]
            .as_array()
            .expect("decisions")
            .len(),
        2,
        "both decisions are still carried verbatim"
    );
}

#[test]
fn agent_version_reports_the_agent_release_not_the_spec() {
    let now = sim_now();
    let metadata = OTSMetadata::new("task", "agent-1", OutcomeType::Success, now)
        .with_spec_version("sha256:abcd")
        .with_harness("temperpaw");
    let trajectory = OTSTrajectory::new(metadata.clone())
        .with_system_message(OTSSystemMessage::new("be helpful", now));

    let atif = to_atif(&trajectory, None).expect("export");
    assert_eq!(
        atif.agent.version, UNKNOWN_AGENT_VERSION,
        "a spec hash is not an agent release"
    );
    assert_eq!(atif.agent.extra["temper.spec_version"], "sha256:abcd");

    let versioned = OTSTrajectory::new(metadata.with_agent_version("temperpaw 3.2"))
        .with_system_message(OTSSystemMessage::new("be helpful", now));
    assert_eq!(
        to_atif(&versioned, None).expect("export").agent.version,
        "temperpaw 3.2"
    );
}
