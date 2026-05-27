use super::helpers::*;
use super::*;

#[test]
fn test_directed_evolution_episode_start_request_materializes_contract() {
    let handle = std::thread::Builder::new()
        .name("directed-evolution-episode-start-request".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("build runtime")
                .block_on(episode_start_request_materializes_contract_body());
        })
        .expect("spawn directed evolution episode start request test");
    if let Err(payload) = handle.join() {
        std::panic::resume_unwind(payload);
    }
}

async fn episode_start_request_materializes_contract_body() {
    let state = PlatformState::new(None);
    install_os_app(
        &state,
        "test-directed-evolution-episode-start-request",
        "directed-evolution",
    )
    .await
    .expect("install directed-evolution");
    let tenant = TenantId::new("test-directed-evolution-episode-start-request");
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

    let direction_id = "dir-growth";
    directed_evolution_create(&state, &tenant, "Direction", direction_id).await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "Direction",
        direction_id,
        "ProposeDirection",
        serde_json::json!({
            "OrganismId": organism_id,
            "PressureIdsJson": "[]",
            "PressureClass": "growth",
            "Title": "Grow answer comparison",
            "Summary": "Human and Codex want answer comparison before acceptance.",
            "ProvenanceJson": serde_json::json!({
                "source": "codex-chat",
                "basis": "human-directed growth"
            }).to_string(),
            "AutonomyLane": "growth-human-gated",
            "ProposedAdaptationGoal": "Help users compare candidate answers and accept with confidence.",
            "ProposedViabilityConstraintsJson": serde_json::json!([
                "Existing answer creation and acceptance must keep working."
            ]).to_string(),
            "BrainRunId": "chat-codex",
        }),
        false,
    )
    .await;

    let request_id = "episode-start-growth";
    directed_evolution_create(&state, &tenant, "EpisodeStartRequest", request_id).await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "EpisodeStartRequest",
        request_id,
        "SubmitEpisodeStartRequest",
        serde_json::json!({
            "DirectionId": direction_id,
            "OrganismId": organism_id,
            "ParentVersionId": parent_version_id,
            "AutonomyLane": "growth-human-gated",
            "RequestedBy": "codex-chat",
            "AdaptationGoal": "Help users compare candidate answers and accept with confidence.",
            "HumanNotes": "Human and Codex negotiated this in chat.",
            "ViabilityConstraintsJson": serde_json::json!([
                { "statement": "Existing answer creation and acceptance must keep working.", "kind": "regression" },
                { "statement": "Variants must not modify evaluators or viability constraints.", "kind": "evaluator-boundary" }
            ]).to_string(),
            "MetricsJson": serde_json::json!([
                {
                    "name": "simulated_user_confidence",
                    "kind": "simulated_user",
                    "unit": "score",
                    "higher_is_better": "true",
                    "description": "AI simulated users report clearer confidence before accepting an answer."
                }
            ]).to_string(),
            "EvaluationStagesJson": serde_json::json!([
                {
                    "name": "Code and spec review",
                    "kind": "reviewer",
                    "executor": "codex",
                    "required_evidence": ["changed_files", "verification_notes"]
                },
                {
                    "name": "AI simulated user growth trial",
                    "kind": "simulated_user",
                    "executor": "codex",
                    "required_evidence": ["simulated_user_trace", "datadog_evidence_scope"]
                }
            ]).to_string(),
            "EliminationRulesJson": serde_json::json!([
                {
                    "statement": "Eliminate variants that fail review, regress baseline behavior, or fail the simulated-user trial.",
                    "metric_names": ["simulated_user_confidence"],
                    "threshold": { "baseline_regression_count": 0 }
                }
            ]).to_string(),
            "ScoringRulesJson": serde_json::json!([
                {
                    "statement": "Prefer the variant with the clearest answer comparison and no baseline regression.",
                    "metric_names": ["simulated_user_confidence"],
                    "weight": "1.0"
                }
            ]).to_string(),
            "SelectionStatement": "Select the variant that improves answer comparison while preserving current behavior.",
            "ContractJson": serde_json::json!({
                "source": "codex-chat",
                "version": "directed-evolution.episode-contract.v1"
            }).to_string(),
            "StartedBy": "codex-chat",
            "Reason": "Start the agreed human-gated growth episode.",
        }),
        true,
    )
    .await;

    let request =
        directed_evolution_entity(&state, &tenant, "EpisodeStartRequest", request_id).await;
    assert_eq!(request.state.status, "Started");
    let episode_id = directed_evolution_field(
        &state,
        &tenant,
        "EpisodeStartRequest",
        request_id,
        "EpisodeId",
    )
    .await;
    assert!(
        !episode_id.is_empty(),
        "request should record the materialized episode id"
    );

    let episode = directed_evolution_entity(&state, &tenant, "Episode", &episode_id).await;
    assert_eq!(episode.state.status, "Running");
    assert_eq!(
        directed_evolution_field(&state, &tenant, "Episode", &episode_id, "AutonomyLane").await,
        "growth-human-gated"
    );
    assert!(
        !directed_evolution_field(&state, &tenant, "Episode", &episode_id, "AdaptationGoalId")
            .await
            .is_empty()
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
        .is_empty()
    );

    let direction = directed_evolution_entity(&state, &tenant, "Direction", direction_id).await;
    assert_eq!(direction.state.status, "Selected");
    assert_eq!(
        directed_evolution_field(&state, &tenant, "Direction", direction_id, "EpisodeId").await,
        episode_id
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
}
