//! Golden tests for the OTS -> ATIF v1.7 export.
//!
//! Three shapes are pinned:
//!
//! 1. A fully populated trajectory — every OTS field the mapping touches is
//!    set, and the resulting ATIF document is asserted field by field.
//! 2. A minimal trajectory — nothing optional is set beyond the one step ATIF
//!    requires, and the export must still be a valid ATIF document with the
//!    optional keys absent rather than present-and-null.
//! 3. A trajectory with nothing in it at all, which is not exportable: ATIF
//!    v1.7 models a trajectory as its step list and has no valid stepless
//!    document.

use temper_ots::atif::{ATIF_SCHEMA_VERSION, AtifExportError, UNKNOWN_AGENT_VERSION, to_atif};
use temper_ots::models::{
    ContentType, DecisionType, MessageRole, OTSAlternative, OTSChoice, OTSConsequence, OTSContext,
    OTSCreditAssignment, OTSDecision, OTSDecisionEvaluation, OTSEntity, OTSMessage,
    OTSMessageContent, OTSMetadata, OTSSystemMessage, OTSTrajectory, OTSTurn, OutcomeType,
};

fn at(offset_seconds: i64) -> chrono::DateTime<chrono::Utc> {
    "2026-04-28T10:00:00Z"
        .parse::<chrono::DateTime<chrono::Utc>>()
        .expect("fixture timestamp parses")
        + chrono::Duration::seconds(offset_seconds)
}

/// A trajectory with every mapped OTS field populated.
fn full_trajectory() -> OTSTrajectory {
    let metadata = OTSMetadata::new(
        "Refund the customer's last order",
        "agent-support-1",
        OutcomeType::Success,
        at(0),
    )
    .with_domain("customer_support")
    .with_timestamp_end(at(30))
    .with_duration_ms(30_000.0)
    .with_framework("langchain")
    .with_harness("temperpaw")
    .with_agent_version("temperpaw 3.2")
    .with_spec_version("sha256:9f2c")
    .with_environment("production")
    .with_feedback_score(0.9)
    .with_human_reviewed(true)
    .with_tag("high_priority")
    .with_parent_trajectory_id("traj-parent-0001");

    let decision = OTSDecision::new(
        DecisionType::ToolSelection,
        OTSChoice::new("RefundOrder")
            .with_arguments(serde_json::json!({"order_id": "order-7"}))
            .with_rationale("The order is already returned")
            .with_confidence(0.95),
        OTSConsequence::success().with_result_summary("Refund issued: $42.00"),
    )
    .with_decision_id("decision-1")
    .with_cause_id("call-abc")
    .with_alternatives(
        "tools",
        vec![OTSAlternative::new("CancelOrder").with_rejected_reason("Order already shipped")],
    )
    .with_evaluation(OTSDecisionEvaluation::new("model_eval", 0.9))
    .with_credit_assignment(OTSCreditAssignment::new(0.8).with_pivotal(true));

    let turn = OTSTurn::new(1, at(5))
        .with_span_id("span-1")
        .with_parent_span_id("span-0")
        .with_duration_ms(1_250.0)
        .with_turn_reward(0.75)
        .with_message(
            OTSMessage::new(
                MessageRole::User,
                OTSMessageContent::text("Please refund my order"),
                at(5),
            )
            .with_message_id("message-user-1"),
        )
        .with_message(
            OTSMessage::new(
                MessageRole::Assistant,
                OTSMessageContent::text("Issuing the refund now."),
                at(6),
            )
            .with_message_id("message-assistant-1")
            .with_reasoning("The order is in Returned, so RefundOrder is legal."),
        )
        .with_message(
            OTSMessage::new(
                MessageRole::Assistant,
                OTSMessageContent::widget(serde_json::json!({"kind": "refund_receipt"})),
                at(7),
            )
            .with_message_id("message-assistant-2"),
        )
        .with_message(
            OTSMessage::new(
                MessageRole::Tool,
                OTSMessageContent::tool_response(serde_json::json!({"status": "ok"})),
                at(8),
            )
            .with_message_id("message-tool-1"),
        )
        .with_decision(decision)
        .with_prompt_token_ids(vec![10, 11, 12])
        .with_completion_token_ids(vec![20, 21])
        .with_response_mask(vec![1, 1])
        .with_logprobs(vec![-0.1, -0.2]);

    OTSTrajectory::new(metadata)
        .with_trajectory_id("traj-full-0001")
        .with_context(OTSContext::new().with_entity(OTSEntity::new("tool", "refund")))
        .with_system_message(OTSSystemMessage::new("You are a support agent", at(0)))
        .with_turn(turn)
        .with_final_reward(0.95)
}

#[test]
fn full_trajectory_exports_field_by_field() {
    let atif = to_atif(&full_trajectory(), Some("session-42")).expect("export");

    // -- Root ------------------------------------------------------------
    assert_eq!(atif.schema_version, ATIF_SCHEMA_VERSION);
    assert_eq!(atif.schema_version, "ATIF-v1.7");
    assert_eq!(atif.session_id.as_deref(), Some("session-42"));
    assert_eq!(atif.trajectory_id.as_deref(), Some("traj-full-0001"));
    assert!(
        atif.subagent_trajectories.is_none(),
        "OTS points child -> parent, so a single document embeds no subagents"
    );

    // -- Agent -----------------------------------------------------------
    assert_eq!(
        atif.agent.name, "temperpaw",
        "harness names the agent system and outranks framework"
    );
    assert_eq!(
        atif.agent.version, "temperpaw 3.2",
        "agent.version is the agent system's release, never the spec hash"
    );
    assert!(
        atif.agent.model_name.is_none(),
        "OTS records no model identity"
    );
    assert_eq!(atif.agent.extra["temper.agent_id"], "agent-support-1");
    assert_eq!(atif.agent.extra["temper.framework"], "langchain");
    assert_eq!(atif.agent.extra["temper.harness"], "temperpaw");
    assert_eq!(atif.agent.extra["temper.spec_version"], "sha256:9f2c");

    // -- Steps -----------------------------------------------------------
    assert_eq!(atif.steps.len(), 3);

    let system = &atif.steps[0];
    assert_eq!(system.step_id, 1);
    assert_eq!(
        serde_json::to_value(system.source).unwrap(),
        serde_json::json!("system")
    );
    assert_eq!(system.message, "You are a support agent");
    assert_eq!(
        system.timestamp.as_deref(),
        Some("2026-04-28T10:00:00+00:00")
    );
    assert!(system.tool_calls.is_empty());
    assert!(system.observation.is_none());
    assert!(system.metrics.is_none());

    let user = &atif.steps[1];
    assert_eq!(user.step_id, 2);
    assert_eq!(
        serde_json::to_value(user.source).unwrap(),
        serde_json::json!("user")
    );
    assert_eq!(user.message, "Please refund my order");
    assert_eq!(user.timestamp.as_deref(), Some("2026-04-28T10:00:05+00:00"));

    let agent = &atif.steps[2];
    assert_eq!(agent.step_id, 3);
    assert_eq!(
        serde_json::to_value(agent.source).unwrap(),
        serde_json::json!("agent")
    );
    assert_eq!(agent.message, "Issuing the refund now.");
    assert_eq!(
        agent.reasoning_content.as_deref(),
        Some("The order is in Returned, so RefundOrder is legal.")
    );
    assert_eq!(
        agent.timestamp.as_deref(),
        Some("2026-04-28T10:00:05+00:00")
    );

    // -- Tool calls ------------------------------------------------------
    assert_eq!(agent.tool_calls.len(), 1);
    let call = &agent.tool_calls[0];
    assert_eq!(
        call.tool_call_id, "call-abc",
        "cause_id is the provider tool-call id and wins over decision_id"
    );
    assert_eq!(call.function_name, "RefundOrder");
    assert_eq!(call.arguments, serde_json::json!({"order_id": "order-7"}));

    // -- Observation -----------------------------------------------------
    let observation = agent.observation.as_ref().expect("observation");
    assert_eq!(observation.results.len(), 2);
    assert_eq!(
        observation.results[0].source_call_id.as_deref(),
        Some("call-abc")
    );
    assert_eq!(
        observation.results[0].content.as_deref(),
        Some("Refund issued: $42.00")
    );
    assert_eq!(
        observation.results[0].extra["temper.consequence"],
        serde_json::json!({"success": true, "error_type": null})
    );
    assert!(
        observation.results[1].source_call_id.is_none(),
        "OTS tool messages carry no call linkage"
    );
    assert_eq!(
        observation.results[1].content.as_deref(),
        Some(r#"{"status":"ok"}"#)
    );

    // -- Metrics ---------------------------------------------------------
    let metrics = agent.metrics.as_ref().expect("metrics");
    assert_eq!(metrics.prompt_token_ids.as_deref(), Some(&[10, 11, 12][..]));
    assert_eq!(metrics.completion_token_ids.as_deref(), Some(&[20, 21][..]));
    assert_eq!(metrics.logprobs.as_deref(), Some(&[-0.1, -0.2][..]));
    assert_eq!(
        metrics.prompt_tokens,
        Some(3),
        "counts are derived from the token-id arrays"
    );
    assert_eq!(metrics.completion_tokens, Some(2));
    assert!(metrics.cost_usd.is_none(), "OTS records no cost");
    assert_eq!(
        metrics.extra["temper.response_mask"],
        serde_json::json!([1, 1])
    );
    assert_eq!(metrics.extra["temper.turn_reward"], serde_json::json!(0.75));

    // -- Step extra ------------------------------------------------------
    assert_eq!(agent.extra["temper.turn_id"], serde_json::json!(1));
    assert_eq!(agent.extra["temper.span_id"], "span-1");
    assert_eq!(agent.extra["temper.parent_span_id"], "span-0");
    assert_eq!(
        agent.extra["temper.duration_ms"],
        serde_json::json!(1_250.0)
    );
    assert!(
        !agent.extra.contains_key("temper.error"),
        "a clean turn records no error flag"
    );

    let decisions = agent.extra["temper.decisions"]
        .as_array()
        .expect("decisions array");
    assert_eq!(decisions.len(), 1);
    assert_eq!(decisions[0]["decision_id"], "decision-1");
    assert_eq!(decisions[0]["decision_type"], "tool_selection");
    assert_eq!(
        decisions[0]["choice"]["confidence"],
        serde_json::json!(0.95)
    );
    assert_eq!(
        decisions[0]["choice"]["rationale"],
        "The order is already returned"
    );
    assert_eq!(
        decisions[0]["alternatives"]["tools"][0]["action"],
        "CancelOrder"
    );
    assert_eq!(decisions[0]["evaluation"]["evaluator_id"], "model_eval");
    assert_eq!(
        decisions[0]["credit_assignment"]["impact"],
        serde_json::json!(0.8)
    );

    let assistant_content = agent.extra["temper.assistant_content"]
        .as_array()
        .expect("assistant content array");
    assert_eq!(
        assistant_content.len(),
        1,
        "only non-text assistant payloads land in extra; text is the step message"
    );
    assert_eq!(
        assistant_content[0]["type"],
        serde_json::to_value(ContentType::Widget).unwrap()
    );

    // -- Final metrics ---------------------------------------------------
    let final_metrics = atif.final_metrics.as_ref().expect("final metrics");
    assert_eq!(final_metrics.total_prompt_tokens, Some(3));
    assert_eq!(final_metrics.total_completion_tokens, Some(2));
    assert_eq!(final_metrics.total_steps, Some(3));
    assert!(final_metrics.total_cost_usd.is_none());

    // -- Root extra ------------------------------------------------------
    assert_eq!(atif.extra["temper.ots_version"], "0.1.0");
    assert_eq!(atif.extra["temper.final_reward"], serde_json::json!(0.95));
    let metadata = &atif.extra["temper.metadata"];
    assert_eq!(
        metadata["task_description"],
        "Refund the customer's last order"
    );
    assert_eq!(metadata["outcome"], "success");
    assert_eq!(metadata["spec_version"], "sha256:9f2c");
    assert_eq!(metadata["harness"], "temperpaw");
    assert_eq!(metadata["domain"], "customer_support");
    assert_eq!(metadata["environment"], "production");
    assert_eq!(metadata["feedback_score"], serde_json::json!(0.9));
    assert_eq!(metadata["human_reviewed"], serde_json::json!(true));
    assert_eq!(metadata["tags"], serde_json::json!(["high_priority"]));
    assert_eq!(metadata["parent_trajectory_id"], "traj-parent-0001");
    assert_eq!(atif.extra["temper.context"]["entities"][0]["id"], "refund");
}

#[test]
fn minimal_trajectory_exports_valid_atif_with_optionals_absent() {
    let metadata = OTSMetadata::new("minimal task", "agent-2", OutcomeType::Failure, at(0));
    // One step is the ATIF floor, so the smallest exportable trajectory is a
    // system message and nothing else.
    let trajectory = OTSTrajectory::new(metadata)
        .with_trajectory_id("traj-minimal-0001")
        .with_system_message(OTSSystemMessage::new("be brief", at(0)));

    let atif = to_atif(&trajectory, None).expect("a system message is a step");
    assert_eq!(atif.schema_version, "ATIF-v1.7");
    assert!(atif.session_id.is_none());
    assert_eq!(atif.steps.len(), 1);
    assert_eq!(
        atif.agent.name, "agent-2",
        "with neither harness nor framework the agent id names the system"
    );
    assert_eq!(
        atif.agent.version, UNKNOWN_AGENT_VERSION,
        "an unreported agent release is unknown, not the OTS format version"
    );

    let json = serde_json::to_value(&atif).expect("serialize ATIF");
    let object = json.as_object().expect("ATIF root is an object");
    assert_eq!(object["schema_version"], "ATIF-v1.7");
    assert_eq!(object["trajectory_id"], "traj-minimal-0001");
    assert_eq!(
        object["steps"].as_array().expect("steps").len(),
        1,
        "ATIF v1.7 requires at least one step"
    );

    // Optional keys must be absent, not present-and-null.
    for absent in ["session_id", "subagent_trajectories"] {
        assert!(
            !object.contains_key(absent),
            "unset optional `{absent}` must be omitted, not null"
        );
    }
    assert!(
        !object["agent"]
            .as_object()
            .unwrap()
            .contains_key("model_name")
    );

    // A trajectory with no turns still reports its step total.
    let final_metrics = &object["final_metrics"];
    assert_eq!(final_metrics["total_steps"], serde_json::json!(1));
    assert!(
        !final_metrics
            .as_object()
            .unwrap()
            .contains_key("total_prompt_tokens"),
        "no turn carried token ids, so no total is invented"
    );

    // No context and no final reward: neither key is fabricated.
    let extra = object["extra"].as_object().expect("root extra");
    assert!(!extra.contains_key("temper.context"));
    assert!(!extra.contains_key("temper.final_reward"));
    assert_eq!(extra["temper.ots_version"], "0.1.0");
}

#[test]
fn a_trajectory_with_no_steps_is_refused_rather_than_exported_stepless() {
    let metadata = OTSMetadata::new("nothing ran", "agent-2", OutcomeType::Failure, at(0));
    let trajectory = OTSTrajectory::new(metadata).with_trajectory_id("traj-empty-0001");

    assert_eq!(
        to_atif(&trajectory, Some("session-empty")),
        Err(AtifExportError::NoSteps),
        "a session finalized before anything ran has no valid ATIF document"
    );
}

#[test]
fn thinking_decisions_do_not_become_environment_interactions() {
    let metadata = OTSMetadata::new("think then act", "agent-4", OutcomeType::Success, at(0));
    let reasoning = OTSDecision::new(
        DecisionType::ReasoningStep,
        OTSChoice::new("compare shipping options"),
        OTSConsequence::success(),
    )
    .with_decision_id("decision-think");
    let response = OTSDecision::new(
        DecisionType::ResponseFormulation,
        OTSChoice::new("apologise and offer a refund"),
        OTSConsequence::success(),
    )
    .with_decision_id("decision-say");
    let invocation = OTSDecision::new(
        DecisionType::ParameterChoice,
        OTSChoice::new("ShipOrder").with_arguments(serde_json::json!({"carrier": "dhl"})),
        OTSConsequence::success().with_result_summary("Shipped"),
    )
    .with_decision_id("decision-ship");

    let trajectory = OTSTrajectory::new(metadata)
        .with_trajectory_id("traj-thinking-0001")
        .with_turn(
            OTSTurn::new(1, at(1))
                .with_span_id("span-1")
                .with_decision(reasoning)
                .with_decision(response)
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
        vec!["ShipOrder"],
        "only tool selections and parameter choices name a callable"
    );
    let results = &step.observation.as_ref().expect("observation").results;
    assert_eq!(
        results.len(),
        1,
        "a thought produces no environment observation"
    );
    assert_eq!(results[0].source_call_id.as_deref(), Some("decision-ship"));
    assert_eq!(
        step.extra["temper.decisions"]
            .as_array()
            .expect("decisions")
            .len(),
        3,
        "every decision is still carried verbatim in extra"
    );
}

#[test]
fn errored_turn_and_fallback_call_id_are_recorded() {
    let metadata = OTSMetadata::new("failing task", "agent-3", OutcomeType::Failure, at(0));
    let decision = OTSDecision::new(
        DecisionType::ToolSelection,
        OTSChoice::new("ShipOrder"),
        OTSConsequence::failure().with_error_type("IllegalTransition"),
    )
    .with_decision_id("decision-9");
    let turn = OTSTurn::new(1, at(1))
        .with_span_id("span-9")
        .with_error(true)
        .with_decision(decision);
    let trajectory = OTSTrajectory::new(metadata)
        .with_trajectory_id("traj-error-0001")
        .with_turn(turn);

    let atif = to_atif(&trajectory, Some("session-9")).expect("export");
    assert_eq!(atif.steps.len(), 1);
    let step = &atif.steps[0];
    assert_eq!(step.message, "", "no assistant text means an empty message");
    assert_eq!(step.extra["temper.error"], serde_json::json!(true));
    assert_eq!(
        step.tool_calls[0].tool_call_id, "decision-9",
        "without cause_id the decision id keeps the call correlated"
    );
    assert_eq!(step.tool_calls[0].arguments, serde_json::json!({}));

    let results = &step.observation.as_ref().expect("observation").results;
    assert_eq!(results[0].source_call_id.as_deref(), Some("decision-9"));
    assert!(results[0].content.is_none());
    assert_eq!(
        results[0].extra["temper.consequence"],
        serde_json::json!({"success": false, "error_type": "IllegalTransition"})
    );
    assert!(
        step.metrics.is_none(),
        "a turn with no token signal and no reward emits no metrics block"
    );
}
