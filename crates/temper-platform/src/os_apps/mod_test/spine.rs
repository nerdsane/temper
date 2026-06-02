use super::helpers::*;
use super::*;

#[test]
fn test_directed_evolution_signal_to_promotion_wasm_spine() {
    if skip_without_genesis_apps("test_directed_evolution_signal_to_promotion_wasm_spine") {
        return;
    }
    let handle = std::thread::Builder::new()
        .name("directed-evolution-spine".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("build runtime")
                .block_on(directed_evolution_signal_to_promotion_wasm_spine_body());
        })
        .expect("spawn directed evolution spine test");
    if let Err(payload) = handle.join() {
        std::panic::resume_unwind(payload);
    }
}

async fn directed_evolution_signal_to_promotion_wasm_spine_body() {
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
            "evidence_scope": [{
                "surface": "logs",
                "query": "service:temper-platform @directed_evolution.organism_id:org-agent-answers",
                "time_window": "2026-06-01T21:00:00Z/2026-06-01T21:10:00Z",
                "result_count": 3,
                "interpretation": "Datadog contained correlated simulated-user runtime requests for the organism.",
                "zero_result_meaning": "failure",
                "datadog_url": "https://app.datadoghq.com/logs?query=service%3Atemper-platform"
            }],
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
            "CreatedByWorkerRunId": "chat-codex",
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
            "CreatedByWorkerRunId": "chat-codex",
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
            "CreatedByWorkerRunId": "chat-codex",
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
    let generation_id = &generation_ids[0];
    let variant_work_items = directed_evolution_wait_for_ids_with_field(
        &state,
        &tenant,
        "WorkItem",
        "Role",
        "variant_generator",
        3,
    )
    .await;
    for (index, work_item_id) in variant_work_items.iter().enumerate() {
        directed_evolution_run_work_item(
            &state,
            &tenant,
            work_item_id,
            "variant_generator",
            serde_json::json!({
                "summary": format!("Variant {}", index + 1),
                "app_ref": format!("agent-answers@variant-{}", index + 1),
                "branch_ref": format!("directed-evolution/variant-{}", index + 1),
                "runtime_ref": format!("http://variant-{}.local", index + 1),
                "changed_files": ["web/src/routes/answers.ts"],
                "diff_ref": format!("diff://variant-{}", index + 1),
            }),
        )
        .await;
    }

    let winner_ids = directed_evolution_wait_for_ids_with_field(
        &state,
        &tenant,
        "Variant",
        "Summary",
        "Variant 1",
        1,
    )
    .await;
    let winner_variant_id = winner_ids[0].clone();
    let evaluation_work_items = {
        let mut ids =
            directed_evolution_ids_with_field(&state, &tenant, "WorkItem", "Role", "reviewer")
                .await;
        ids.extend(
            directed_evolution_ids_with_field(
                &state,
                &tenant,
                "WorkItem",
                "Role",
                "simulated_user",
            )
            .await,
        );
        ids
    };
    assert_eq!(evaluation_work_items.len(), 6);

    let mut eliminated_variants = BTreeSet::new();
    for work_item_id in evaluation_work_items {
        let role =
            directed_evolution_field(&state, &tenant, "WorkItem", &work_item_id, "Role").await;
        let target_entity_id =
            directed_evolution_field(&state, &tenant, "WorkItem", &work_item_id, "TargetEntityId")
                .await;
        let stage_result_id = if role == "simulated_user" {
            directed_evolution_field(&state, &tenant, "Trial", &target_entity_id, "StageResultId")
                .await
        } else {
            target_entity_id
        };
        let variant_id = directed_evolution_field(
            &state,
            &tenant,
            "StageResult",
            &stage_result_id,
            "VariantId",
        )
        .await;
        if variant_id == winner_variant_id {
            if role == "simulated_user" {
                directed_evolution_run_work_item(
                    &state,
                    &tenant,
                    &work_item_id,
                    &role,
                    serde_json::json!({
                        "status": "observed",
                        "summary": "User could compare answers and complete acceptance.",
                        "journey": [
                            {"step": "Compare candidate answers", "result": "Comparison cues were visible."},
                            {"step": "Accept best answer", "result": "Acceptance completed."}
                        ],
                        "observations": {
                            "what_happened": "The variant supported the intended comparison journey.",
                            "unmet_intents": []
                        },
                        "intent_satisfied": "yes",
                        "friction": [],
                        "metrics": {
                            "observed_latency_ms": {"value": 120, "unit": "ms", "provenance_kind": "agent-observed"}
                        },
                        "evidence_scope": [
                            {"surface": "runtime", "query": "/tdata", "result_summary": "OData runtime was reachable."}
                        ],
                    }),
                )
                .await;
            } else {
                directed_evolution_run_work_item(
                    &state,
                    &tenant,
                    &work_item_id,
                    &role,
                    serde_json::json!({
                        "passed": true,
                        "status": "passed",
                        "summary": "Variant keeps baseline behavior and improves comparison clarity.",
                        "metrics": {
                            "clarity_score": 0.91,
                            "regression_count": 0
                        },
                    }),
                )
                .await;
            }
        } else if eliminated_variants.insert(variant_id.clone()) {
            directed_evolution_run_work_item(
                &state,
                &tenant,
                &work_item_id,
                &role,
                serde_json::json!({
                    "passed": false,
                    "status": "failed",
                    "summary": "Variant regressed the baseline acceptance path.",
                    "failure_reason": "baseline regression",
                    "metrics": {
                        "clarity_score": 0.52,
                        "regression_count": 1
                    },
                }),
            )
            .await;
        }
    }

    let viability_work_items = directed_evolution_wait_for_ids_with_field(
        &state,
        &tenant,
        "WorkItem",
        "Role",
        "viability_evaluator",
        1,
    )
    .await;
    for work_item_id in viability_work_items {
        let stage_result_id =
            directed_evolution_field(&state, &tenant, "WorkItem", &work_item_id, "TargetEntityId")
                .await;
        let variant_id = directed_evolution_field(
            &state,
            &tenant,
            "StageResult",
            &stage_result_id,
            "VariantId",
        )
        .await;
        if variant_id == winner_variant_id {
            directed_evolution_run_work_item(
                &state,
                &tenant,
                &work_item_id,
                "viability_evaluator",
                serde_json::json!({
                    "passed": true,
                    "status": "passed",
                    "summary": "Recorded simulated-user observations support the Adaptation Goal with no regression.",
                    "metrics": {
                        "intent_satisfaction": {"value": 1.0, "unit": "ratio", "provenance_kind": "brain-judged"},
                        "trial_blocker_count": {"value": 0, "unit": "count", "provenance_kind": "state-verified"}
                    },
                    "decision_basis": {"why": "The trial completed the intended journey and no blockers were recorded."},
                }),
            )
            .await;
        }
    }

    let selector_work_items = directed_evolution_wait_for_ids_with_field(
        &state, &tenant, "WorkItem", "Role", "selector", 1,
    )
    .await;
    directed_evolution_run_work_item(
        &state,
        &tenant,
        &selector_work_items[0],
        "selector",
        serde_json::json!({
            "winning_variant_id": winner_variant_id,
            "selection_explanation": "Variant 1 won on clarity while preserving the baseline.",
            "app_ref": "agent-answers@variant-1",
            "commit_ref": "directed-evolution/variant-1",
            "evidence_uri": "evidence://selection/variant-1",
            "digest": "selection-digest",
        }),
    )
    .await;

    assert_eq!(
        directed_evolution_entity(&state, &tenant, "Episode", episode_id)
            .await
            .state
            .status,
        "Completed"
    );
    assert_eq!(
        directed_evolution_entity(&state, &tenant, "Generation", generation_id)
            .await
            .state
            .status,
        "Completed"
    );
    assert_eq!(
        directed_evolution_entity(&state, &tenant, "Variant", &winner_variant_id)
            .await
            .state
            .status,
        "Promoted"
    );
    assert_eq!(
        directed_evolution_entity(&state, &tenant, "OrganismVersion", parent_version_id)
            .await
            .state
            .status,
        "Superseded"
    );
    assert_eq!(state.server.list_entity_ids(&tenant, "Promotion").len(), 1);
    assert_eq!(
        state.server.list_entity_ids(&tenant, "LineageEdge").len(),
        1
    );
    assert!(
        !state
            .server
            .list_entity_ids(&tenant, "Measurement")
            .is_empty(),
        "stage evaluation should create measurement entities"
    );
    let mut succeeded_trial = false;
    for trial_id in state.server.list_entity_ids(&tenant, "Trial") {
        if directed_evolution_entity(&state, &tenant, "Trial", &trial_id)
            .await
            .state
            .status
            == "Succeeded"
        {
            succeeded_trial = true;
            break;
        }
    }
    assert!(
        succeeded_trial,
        "winner's simulated-user stage should complete a trial"
    );
}
