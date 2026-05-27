struct SpineSetup {
    state: PlatformState,
    tenant: TenantId,
    parent_version_id: &'static str,
    episode_id: &'static str,
    generation_id: String,
}

async fn setup_directed_evolution_spine() -> SpineSetup {
    let state = PlatformState::new(None);
    install_os_app(
        &state,
        "test-directed-evolution-spine",
        "directed-evolution",
    )
    .await
    .expect("install directed-evolution");
    let tenant = TenantId::new("test-directed-evolution-spine");
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

    directed_evolution_create(&state, &tenant, "Signal", "sig-growth").await;
    let signal_response = directed_evolution_dispatch(
        &state,
        &tenant,
        "Signal",
        "sig-growth",
        "RecordSignal",
        serde_json::json!({
            "Source": "datadog-and-simulated-users",
            "SignalKind": "growth_pressure",
            "OrganismId": organism_id,
            "Summary": "Simulated users repeatedly compare answers before accepting one.",
            "EvidenceArtifactId": "",
            "CorrelationJson": "{\"sessions\":3}",
        }),
        true,
    )
    .await;
    assert!(
        signal_response
            .custom_effects
            .iter()
            .any(|effect| effect.ends_with(":signal_recorded")),
        "RecordSignal should emit signal_recorded, got {:?}; final signal state {:?}",
        signal_response.custom_effects,
        signal_response.state
    );

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
            "pressure_class": "growth",
            "pressure_summary": "Users need clearer answer comparison before accepting.",
            "title": "Grow accepted-answer comparison",
            "direction_summary": "Add evidence-aware answer comparison before acceptance.",
            "autonomy_lane": "human-approval",
            "proposed_adaptation_goal": "Help users compare candidate answers and accept with confidence.",
            "proposed_viability_constraints": [
                "Do not regress existing answer creation.",
                "Keep answer acceptance reversible."
            ],
        }),
    )
    .await;

    let direction_ids = directed_evolution_wait_for_ids_with_field(
        &state,
        &tenant,
        "Direction",
        "Title",
        "Grow accepted-answer comparison",
        1,
    )
    .await;
    let direction_id = &direction_ids[0];
    assert_eq!(
        directed_evolution_entity(&state, &tenant, "Signal", "sig-growth")
            .await
            .state
            .status,
        "Linked"
    );

    let episode_id = "ep-growth";
    let goal_id = "goal-growth";
    let selection_pressure_id = "selection-growth";
    let constraint_id = "constraint-no-regression";
    let review_stage_id = "stage-review";
    let sim_stage_id = "stage-simulated-user";
    for (entity_type, entity_id) in [
        ("Episode", episode_id),
        ("AdaptationGoal", goal_id),
        ("SelectionPressure", selection_pressure_id),
        ("ViabilityConstraint", constraint_id),
        ("EvaluationStage", review_stage_id),
        ("EvaluationStage", sim_stage_id),
    ] {
        directed_evolution_create(&state, &tenant, entity_type, entity_id).await;
    }
    directed_evolution_dispatch(
        &state,
        &tenant,
        "Episode",
        episode_id,
        "BeginEpisodeNegotiation",
        serde_json::json!({
            "DirectionId": direction_id,
            "OrganismId": organism_id,
            "ParentVersionId": parent_version_id,
            "AutonomyLane": "human-approval",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "AdaptationGoal",
        goal_id,
        "ActivateAdaptationGoal",
        serde_json::json!({
            "EpisodeId": episode_id,
            "GoalStatement": "Help users compare candidate answers and accept with confidence.",
            "CreatedByBrainRunId": "chat-codex",
            "HumanNotes": "Human and brain agreed in chat.",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "SelectionPressure",
        selection_pressure_id,
        "ActivateSelectionPressure",
        serde_json::json!({
            "EpisodeId": episode_id,
            "SelectionStatement": "Prefer the variant with better evidence clarity and no baseline regression.",
            "MetricIdsJson": "[]",
            "EliminationRuleIdsJson": "[]",
            "ScoringRuleIdsJson": "[]",
            "CreatedByBrainRunId": "chat-codex",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "ViabilityConstraint",
        constraint_id,
        "ActivateViabilityConstraint",
        serde_json::json!({
            "EpisodeId": episode_id,
            "ConstraintStatement": "Existing answer creation and acceptance still work.",
            "ConstraintKind": "regression",
            "CreatedByBrainRunId": "chat-codex",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "EvaluationStage",
        review_stage_id,
        "ActivateEvaluationStage",
        serde_json::json!({
            "EpisodeId": episode_id,
            "StageName": "Code and spec review",
            "StageKind": "reviewer",
            "SequenceIndex": 1,
            "RequiredEvidenceJson": "[]",
            "ExecutorKind": "codex",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "EvaluationStage",
        sim_stage_id,
        "ActivateEvaluationStage",
        serde_json::json!({
            "EpisodeId": episode_id,
            "StageName": "AI simulated user trial",
            "StageKind": "simulated_user",
            "SequenceIndex": 2,
            "RequiredEvidenceJson": "[]",
            "ExecutorKind": "codex",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "Episode",
        episode_id,
        "RecordEpisodeContract",
        serde_json::json!({
            "AdaptationGoalId": goal_id,
            "SelectionPressureId": selection_pressure_id,
            "ViabilityConstraintIdsJson": serde_json::json!([constraint_id]).to_string(),
            "EvaluationStageIdsJson": serde_json::json!([review_stage_id, sim_stage_id]).to_string(),
            "EliminationRuleIdsJson": "[]",
            "ScoringRuleIdsJson": "[]",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "Direction",
        direction_id,
        "SelectDirection",
        serde_json::json!({
            "EpisodeId": episode_id,
            "SelectedBy": "human",
            "SelectionNotes": "Selected through Codex chat.",
        }),
        false,
    )
    .await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "Episode",
        episode_id,
        "StartEpisode",
        serde_json::json!({
            "StartedBy": "codex",
            "Reason": "Start the agreed directed evolution episode.",
        }),
        true,
    )
    .await;

    let generation_ids = directed_evolution_wait_for_ids_with_field(
        &state,
        &tenant,
        "Generation",
        "EpisodeId",
        episode_id,
        1,
    )
    .await;

    SpineSetup {
        state,
        tenant,
        parent_version_id,
        episode_id,
        generation_id: generation_ids[0].clone(),
    }
}
