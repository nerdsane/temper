use super::helpers::*;
use super::*;

#[test]
fn test_directed_evolution_failed_variant_generation_closes_episode() {
    let handle = std::thread::Builder::new()
        .name("directed-evolution-failure-spine".to_string())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_stack_size(16 * 1024 * 1024)
                .enable_all()
                .build()
                .expect("build runtime")
                .block_on(directed_evolution_failed_variant_generation_closes_episode_body());
        })
        .expect("spawn directed evolution failure spine test");
    if let Err(payload) = handle.join() {
        std::panic::resume_unwind(payload);
    }
}

async fn directed_evolution_failed_variant_generation_closes_episode_body() {
    let state = PlatformState::new(None);
    install_os_app(
        &state,
        "test-directed-evolution-failure-spine",
        "directed-evolution",
    )
    .await
    .expect("install directed-evolution");
    let tenant = TenantId::new("test-directed-evolution-failure-spine");
    directed_evolution_register_wasm_modules_for_test(&state, &tenant);

    let organism_id = "org-agent-answers-failure";
    let parent_version_id = "ov-parent-failure";
    let episode_id = "ep-failure";
    let direction_id = "dir-failure";
    directed_evolution_create(&state, &tenant, "Organism", organism_id).await;
    directed_evolution_create(&state, &tenant, "OrganismVersion", parent_version_id).await;
    directed_evolution_create(&state, &tenant, "Direction", direction_id).await;
    directed_evolution_create(&state, &tenant, "Episode", episode_id).await;
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
        "Episode",
        episode_id,
        "BeginEpisodeNegotiation",
        serde_json::json!({
            "DirectionId": direction_id,
            "OrganismId": organism_id,
            "ParentVersionId": parent_version_id,
            "AutonomyLane": "repair-auto",
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
            "AdaptationGoalId": "",
            "SelectionPressureId": "",
            "ViabilityConstraintIdsJson": "[]",
            "EvaluationStageIdsJson": "[]",
            "EliminationRuleIdsJson": "[]",
            "ScoringRuleIdsJson": "[]",
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
            "Reason": "Start the failure-path episode.",
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
    for work_item_id in &variant_work_items {
        directed_evolution_fail_work_item(
            &state,
            &tenant,
            work_item_id,
            "variant_generator",
            "codex worker failed before producing a runnable variant",
        )
        .await;
    }

    assert_eq!(
        directed_evolution_entity(&state, &tenant, "Generation", generation_id)
            .await
            .state
            .status,
        "Failed"
    );
    assert_eq!(
        directed_evolution_entity(&state, &tenant, "Episode", episode_id)
            .await
            .state
            .status,
        "Failed"
    );
    let variants = directed_evolution_wait_for_ids_with_field(
        &state,
        &tenant,
        "Variant",
        "GenerationId",
        generation_id,
        3,
    )
    .await;
    for variant_id in variants {
        assert_eq!(
            directed_evolution_entity(&state, &tenant, "Variant", &variant_id)
                .await
                .state
                .status,
            "Failed"
        );
    }
    assert!(
        directed_evolution_ids_with_field(&state, &tenant, "WorkItem", "Role", "selector")
            .await
            .is_empty(),
        "failed generation must not queue a selector"
    );
}
