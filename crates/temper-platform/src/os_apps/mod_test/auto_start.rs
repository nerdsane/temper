use super::helpers::*;
use super::*;

#[test]
fn test_directed_evolution_repair_direction_autostarts_from_policy() {
    let handle = std::thread::Builder::new()
        .name("directed-evolution-repair-autostart".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("build runtime")
                .block_on(repair_direction_autostarts_from_policy_body());
        })
        .expect("spawn directed evolution repair autostart test");
    if let Err(payload) = handle.join() {
        std::panic::resume_unwind(payload);
    }
}

async fn repair_direction_autostarts_from_policy_body() {
    let state = PlatformState::new(None);
    install_os_app(
        &state,
        "test-directed-evolution-repair-autostart",
        "directed-evolution",
    )
    .await
    .expect("install directed-evolution");
    let tenant = TenantId::new("test-directed-evolution-repair-autostart");
    directed_evolution_register_wasm_modules_for_test(&state, &tenant);

    let organism_id = "org-agent-answers";
    let parent_version_id = "ov-parent";
    directed_evolution_create(&state, &tenant, "Organism", organism_id).await;
    directed_evolution_create(&state, &tenant, "OrganismVersion", parent_version_id).await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "OrganismVersion",
        parent_version_id,
        "MarkOrganismVersionParent",
        serde_json::json!({
            "OrganismId": organism_id,
            "AppRef": "agent-answers@baseline",
            "CommitRef": "baseline",
            "PromotionId": "",
            "Summary": "Baseline Agent Answers organism.",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "Organism",
        organism_id,
        "ActivateOrganism",
        serde_json::json!({
            "Name": "Agent Answers",
            "AppRef": "agent-answers@baseline",
            "ParentVersionId": parent_version_id,
            "BaselineEvaluationJson": "{}",
        }),
        false,
    )
    .await;

    directed_evolution_create(&state, &tenant, "AutonomyPolicy", "policy-repair-auto").await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "AutonomyPolicy",
        "policy-repair-auto",
        "ActivateAutonomyPolicy",
        serde_json::json!({
            "OrganismId": organism_id,
            "PolicyJson": serde_json::json!({
                "repair_lane": "automatic for failing checks, regressions, heavy errors, and performance regressions after required evaluations pass",
                "growth_lane": "human approval required before episode start",
                "policy_lane": "human approval required",
            }).to_string(),
            "CreatedBy": "test",
            "Summary": "Repair can move automatically; growth remains gated.",
        }),
        false,
    )
    .await;

    directed_evolution_create(&state, &tenant, "Signal", "sig-repair").await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "Signal",
        "sig-repair",
        "RecordSignal",
        serde_json::json!({
            "Source": "datadog-and-simulated-users",
            "SignalKind": "repair_pressure",
            "OrganismId": organism_id,
            "Summary": "Simulated users hit a regression in answer acceptance.",
            "EvidenceArtifactId": "",
            "CorrelationJson": "{\"sessions\":3}",
        }),
        true,
    )
    .await;

    let observer_work_items = directed_evolution_wait_for_ids_with_field(
        &state, &tenant, "WorkItem", "Role", "observer", 1,
    )
    .await;
    directed_evolution_run_work_item(
        &state,
        &tenant,
        &observer_work_items[0],
        "observer",
        serde_json::json!({
            "actionable": true,
            "pressure_class": "repair",
            "pressure_summary": "Answer acceptance is failing for normal simulated users.",
            "title": "Repair answer acceptance regression",
            "direction_summary": "Restore baseline answer acceptance while preserving current answer creation.",
            "autonomy_lane": "repair-auto",
            "proposed_adaptation_goal": "Restore answer acceptance for simulated users without changing the product surface.",
            "proposed_viability_constraints": [
                "Question.Configure, Answer.Submit, and Answer.Accept remain available.",
                "Variants must not modify evaluation rules."
            ],
            "evidence_scope": directed_evolution_datadog_evidence_scope(),
            "selection_statement": "Prefer variants that restore acceptance and introduce no baseline regressions.",
            "metric_definitions": [
                {
                    "metric_name": "acceptance_repair_success",
                    "metric_kind": "repair_outcome",
                    "unit": "boolean",
                    "higher_is_better": "true",
                    "description": "Simulated users can accept an answer again."
                }
            ]
        }),
    )
    .await;

    let direction_ids = directed_evolution_wait_for_ids_with_field(
        &state,
        &tenant,
        "Direction",
        "Title",
        "Repair answer acceptance regression",
        1,
    )
    .await;
    let direction_id = &direction_ids[0];
    let episode_id = directed_evolution_wait_for_nonempty_field(
        &state,
        &tenant,
        "Direction",
        direction_id,
        "EpisodeId",
    )
    .await;
    assert_eq!(
        directed_evolution_entity(&state, &tenant, "Direction", direction_id)
            .await
            .state
            .status,
        "Selected"
    );
    let episode = directed_evolution_entity(&state, &tenant, "Episode", &episode_id).await;
    assert_eq!(episode.state.status, "Running");
    assert_eq!(
        directed_evolution_field(&state, &tenant, "Episode", &episode_id, "AutonomyLane").await,
        "repair-auto"
    );
    assert!(
        !directed_evolution_field(&state, &tenant, "Episode", &episode_id, "AdaptationGoalId")
            .await
            .is_empty(),
        "repair autostart should record an Adaptation Goal"
    );
    assert!(
        !directed_evolution_field(
            &state,
            &tenant,
            "Episode",
            &episode_id,
            "SelectionPressureId"
        )
        .await
        .is_empty(),
        "repair autostart should record Selection Pressure"
    );
    let evaluation_stage_ids = directed_evolution_field(
        &state,
        &tenant,
        "Episode",
        &episode_id,
        "EvaluationStageIdsJson",
    )
    .await;
    let evaluation_stage_ids_json: serde_json::Value =
        serde_json::from_str(&evaluation_stage_ids).expect("evaluation stage ids json");
    assert!(
        evaluation_stage_ids_json
            .as_array()
            .map(|items| items.len() >= 2)
            .unwrap_or(false),
        "repair autostart should record evaluation stages, got {evaluation_stage_ids}"
    );

    let generation_ids = directed_evolution_wait_for_ids_with_field(
        &state,
        &tenant,
        "Generation",
        "EpisodeId",
        &episode_id,
        1,
    )
    .await;
    assert_eq!(generation_ids.len(), 1);
    let variant_work_items = directed_evolution_wait_for_ids_with_field(
        &state,
        &tenant,
        "WorkItem",
        "Role",
        "variant_generator",
        3,
    )
    .await;
    assert_eq!(variant_work_items.len(), 3);

    let episode_count_after_repair = state.server.list_entity_ids(&tenant, "Episode").len();
    directed_evolution_create(&state, &tenant, "Signal", "sig-growth-gated").await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "Signal",
        "sig-growth-gated",
        "RecordSignal",
        serde_json::json!({
            "Source": "simulated-user-agent",
            "SignalKind": "growth_pressure",
            "OrganismId": organism_id,
            "Summary": "Users want a new answer comparison product surface.",
            "EvidenceArtifactId": "",
            "CorrelationJson": "{\"sessions\":2}",
        }),
        true,
    )
    .await;
    let growth_observer_work_items = directed_evolution_wait_for_ids_with_field(
        &state,
        &tenant,
        "WorkItem",
        "TargetEntityId",
        "sig-growth-gated",
        1,
    )
    .await;
    directed_evolution_run_work_item(
        &state,
        &tenant,
        &growth_observer_work_items[0],
        "observer",
        serde_json::json!({
            "actionable": true,
            "pressure_class": "growth",
            "pressure_summary": "Users want a new answer comparison surface.",
            "title": "Grow answer comparison surface",
            "direction_summary": "Add a visible comparison surface before answer acceptance.",
            "autonomy_lane": "growth-human-gated",
            "proposed_adaptation_goal": "Let humans compare candidate answers before acceptance.",
            "proposed_viability_constraints": [
                "Do not regress answer acceptance."
            ],
            "evidence_scope": directed_evolution_datadog_evidence_scope(),
        }),
    )
    .await;
    let growth_direction_ids = directed_evolution_wait_for_ids_with_field(
        &state,
        &tenant,
        "Direction",
        "Title",
        "Grow answer comparison surface",
        1,
    )
    .await;
    let growth_direction_id = &growth_direction_ids[0];
    assert_eq!(
        directed_evolution_entity(&state, &tenant, "Direction", growth_direction_id)
            .await
            .state
            .status,
        "Proposed"
    );
    assert_eq!(
        directed_evolution_field(
            &state,
            &tenant,
            "Direction",
            growth_direction_id,
            "EpisodeId"
        )
        .await,
        ""
    );
    assert_eq!(
        state.server.list_entity_ids(&tenant, "Episode").len(),
        episode_count_after_repair,
        "growth directions must remain human-gated even with an active repair-auto policy"
    );
}
