use super::helpers::*;
use super::*;

include!("spine_setup.rs");

#[test]
fn test_directed_evolution_signal_to_promotion_wasm_spine() {
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
    let SpineSetup {
        state,
        tenant,
        parent_version_id,
        episode_id,
        generation_id,
    } = setup_directed_evolution_spine().await;
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
    let secondary_survivor_ids = directed_evolution_wait_for_ids_with_field(
        &state,
        &tenant,
        "Variant",
        "Summary",
        "Variant 2",
        1,
    )
    .await;
    let secondary_survivor_variant_id = secondary_survivor_ids[0].clone();
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
        if variant_id == winner_variant_id || variant_id == secondary_survivor_variant_id {
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
        directed_evolution_entity(&state, &tenant, "Generation", &generation_id)
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
    let secondary_survivor =
        directed_evolution_entity(&state, &tenant, "Variant", &secondary_survivor_variant_id).await;
    assert_eq!(secondary_survivor.state.status, "Eliminated");
    assert!(
        directed_evolution_field(
            &state,
            &tenant,
            "Variant",
            &secondary_survivor_variant_id,
            "Reason"
        )
        .await
        .contains("selection boundary"),
        "non-winning survivor should explain why it was eliminated"
    );
    assert_eq!(
        directed_evolution_entity(&state, &tenant, "OrganismVersion", parent_version_id)
            .await
            .state
            .status,
        "Superseded"
    );
    assert_eq!(state.server.list_entity_ids(&tenant, "Promotion").len(), 1);
    let promoter_work_items = directed_evolution_wait_for_ids_with_field(
        &state, &tenant, "WorkItem", "Role", "promoter", 1,
    )
    .await;
    assert_eq!(
        directed_evolution_field(
            &state,
            &tenant,
            "WorkItem",
            &promoter_work_items[0],
            "TargetEntityType"
        )
        .await,
        "Promotion"
    );
    let promotion_id = state.server.list_entity_ids(&tenant, "Promotion")[0].clone();
    directed_evolution_run_work_item(
        &state,
        &tenant,
        &promoter_work_items[0],
        "promoter",
        serde_json::json!({
            "status": "succeeded",
            "canonical_app_ref": "agent-answers@variant-1",
            "production_tenant": "default",
            "runtime_ref": "temper://tenant/default/app/agent-answers@variant-1",
            "summary": "Published and installed winner.",
            "digest": "variant-1-digest",
            "evidence_refs": ["temper://tenant/default/app/agent-answers@variant-1"],
        }),
    )
    .await;
    let promotion = directed_evolution_entity(&state, &tenant, "Promotion", &promotion_id).await;
    assert_eq!(
        promotion.state.booleans.get("materialized").copied(),
        Some(true)
    );
    assert_eq!(
        directed_evolution_field(&state, &tenant, "Promotion", &promotion_id, "RuntimeRef").await,
        "temper://tenant/default/app/agent-answers@variant-1"
    );
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
