use super::helpers::*;
use super::*;

include!("spine_setup.rs");

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
    let mut eliminated_variants = BTreeSet::new();
    let reviewer_work_items = directed_evolution_wait_for_ids_with_field(
        &state, &tenant, "WorkItem", "Role", "reviewer", 3,
    )
    .await;
    for work_item_id in reviewer_work_items {
        let role =
            directed_evolution_field(&state, &tenant, "WorkItem", &work_item_id, "Role").await;
        let variant_id =
            directed_evolution_work_item_variant_id(&state, &tenant, &work_item_id).await;
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

    let simulated_user_work_items = directed_evolution_wait_for_ids_with_field(
        &state,
        &tenant,
        "WorkItem",
        "Role",
        "simulated_user",
        3,
    )
    .await;
    for work_item_id in simulated_user_work_items {
        let variant_id =
            directed_evolution_work_item_variant_id(&state, &tenant, &work_item_id).await;
        if variant_id == winner_variant_id || variant_id == secondary_survivor_variant_id {
            directed_evolution_run_work_item(
                &state,
                &tenant,
                &work_item_id,
                "simulated_user",
                serde_json::json!({
                    "status": "observed",
                    "summary": "Simulated user completed the answer comparison journey.",
                    "journey": ["opened question", "compared candidate answers", "accepted the clearer answer"],
                    "observations": {
                        "comparison_visible": true,
                        "acceptance_completed": true
                    },
                    "intent_satisfied": "true",
                    "friction": [],
                    "metrics": {
                        "simulated_user_confidence": 0.91,
                        "runtime_probe_count": 3
                    },
                    "blocker": "",
                    "blocker_kind": "none"
                }),
            )
            .await;
        } else {
            directed_evolution_run_work_item(
                &state,
                &tenant,
                &work_item_id,
                "simulated_user",
                serde_json::json!({
                    "status": "blocked",
                    "summary": "Simulated user could not complete answer acceptance.",
                    "journey": ["opened question", "attempted answer acceptance"],
                    "observations": {
                        "comparison_visible": false,
                        "acceptance_completed": false
                    },
                    "intent_satisfied": "false",
                    "friction": ["acceptance path regressed"],
                    "metrics": {
                        "simulated_user_confidence": 0.2,
                        "runtime_probe_count": 2
                    },
                    "blocker": "baseline acceptance path regressed",
                    "blocker_kind": "app-behavior"
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
        3,
    )
    .await;
    for work_item_id in viability_work_items {
        let variant_id =
            directed_evolution_work_item_variant_id(&state, &tenant, &work_item_id).await;
        if variant_id == winner_variant_id || variant_id == secondary_survivor_variant_id {
            directed_evolution_run_work_item(
                &state,
                &tenant,
                &work_item_id,
                "viability_evaluator",
                serde_json::json!({
                    "passed": true,
                    "status": "passed",
                    "summary": "Evaluator confirmed simulated-user observations satisfy the stage.",
                    "metrics": {
                        "simulated_user_confidence": 0.91,
                        "blocked_trial_count": 0
                    },
                    "decision_basis": {
                        "basis": "trial observations and state-verified trial counts"
                    }
                }),
            )
            .await;
        } else {
            directed_evolution_run_work_item(
                &state,
                &tenant,
                &work_item_id,
                "viability_evaluator",
                serde_json::json!({
                    "passed": false,
                    "status": "failed",
                    "summary": "Evaluator found blocked simulated-user trial evidence.",
                    "failure_reason": "simulated-user trial was blocked by app behavior",
                    "metrics": {
                        "simulated_user_confidence": 0.2,
                        "blocked_trial_count": 1
                    },
                    "decision_basis": {
                        "basis": "trial observations and state-verified blocked trial count"
                    }
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
    assert_eq!(secondary_survivor.state.status, "NotSelected");
    assert!(
        directed_evolution_field(
            &state,
            &tenant,
            "Variant",
            &secondary_survivor_variant_id,
            "Reason"
        )
        .await
        .contains("not selected because the winner"),
        "non-winning survivor should explain why it was not selected"
    );
    assert_eq!(
        directed_evolution_entity(&state, &tenant, "OrganismVersion", parent_version_id)
            .await
            .state
            .status,
        "Superseded"
    );
    assert_eq!(
        directed_evolution_field(&state, &tenant, "Organism", "org-agent-answers", "AppRef").await,
        "agent-answers@baseline"
    );
    let organism_before_sync =
        directed_evolution_entity(&state, &tenant, "Organism", "org-agent-answers").await;
    assert_eq!(
        organism_before_sync.state.counters.get("version_count"),
        Some(&2)
    );
    assert_eq!(state.server.list_entity_ids(&tenant, "Promotion").len(), 1);
    let promotion_id = state.server.list_entity_ids(&tenant, "Promotion")[0].clone();
    let promoted_organism_version_id = directed_evolution_field(
        &state,
        &tenant,
        "Promotion",
        &promotion_id,
        "NewOrganismVersionId",
    )
    .await;
    directed_evolution_dispatch(
        &state,
        &tenant,
        "Organism",
        "org-agent-answers",
        "SyncOrganismParentRef",
        serde_json::json!({
            "OrganismVersionId": promoted_organism_version_id,
            "PromotionId": promotion_id,
            "AppRef": "agent-answers@variant-1",
            "Summary": "Idempotent live parent ref sync.",
        }),
        false,
    )
    .await;
    let organism_after_sync =
        directed_evolution_entity(&state, &tenant, "Organism", "org-agent-answers").await;
    assert_eq!(
        organism_after_sync.state.counters.get("version_count"),
        Some(&2)
    );
    assert_eq!(
        directed_evolution_field(&state, &tenant, "Organism", "org-agent-answers", "Summary").await,
        "Idempotent live parent ref sync."
    );
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
    assert_eq!(
        directed_evolution_entity(&state, &tenant, "Episode", episode_id)
            .await
            .state
            .status,
        "Completed"
    );
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
        directed_evolution_field(&state, &tenant, "Organism", "org-agent-answers", "AppRef").await,
        "agent-answers@variant-1"
    );
    assert_eq!(
        directed_evolution_field(&state, &tenant, "Organism", "org-agent-answers", "Summary").await,
        "Published and installed winner."
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

async fn directed_evolution_work_item_variant_id(
    state: &PlatformState,
    tenant: &TenantId,
    work_item_id: &str,
) -> String {
    let target_entity_type =
        directed_evolution_field(state, tenant, "WorkItem", work_item_id, "TargetEntityType").await;
    let target_entity_id =
        directed_evolution_field(state, tenant, "WorkItem", work_item_id, "TargetEntityId").await;
    match target_entity_type.as_str() {
        "StageResult" => {
            directed_evolution_field(state, tenant, "StageResult", &target_entity_id, "VariantId")
                .await
        }
        "Trial" => {
            directed_evolution_field(state, tenant, "Trial", &target_entity_id, "VariantId").await
        }
        _ => String::new(),
    }
}
