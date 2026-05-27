fn route_failed_work_item(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    work_item_id: &str,
    role: &str,
    target_entity_type: &str,
    target_entity_id: &str,
    work_item_fields: &Value,
) -> Result<Value, String> {
    let failure_reason = nonempty(
        field_str(work_item_fields, &["FailureReason"]),
        format!("WorkItem {work_item_id} failed"),
    );
    let evidence_artifact_id = field_str(work_item_fields, &["EvidenceArtifactId"]);

    match (role, target_entity_type) {
        ("observer", "Signal") => route_failed_observer(
            ctx,
            base_url,
            headers,
            target_entity_id,
            &failure_reason,
            &evidence_artifact_id,
        ),
        ("variant_generator", "Generation") => route_failed_variant_generator(
            ctx,
            base_url,
            headers,
            work_item_id,
            target_entity_id,
            work_item_fields,
            &failure_reason,
            &evidence_artifact_id,
        ),
        ("reviewer", "StageResult") | ("simulated_user", "StageResult") => {
            route_failed_stage_result(
                ctx,
                base_url,
                headers,
                work_item_id,
                role,
                target_entity_id,
                &failure_reason,
                &evidence_artifact_id,
            )
        }
        ("selector", "Generation") => route_failed_selector(
            ctx,
            base_url,
            headers,
            target_entity_id,
            &failure_reason,
            &evidence_artifact_id,
        ),
        ("promoter", "Promotion") => route_failed_promoter(
            ctx,
            base_url,
            headers,
            target_entity_id,
            &failure_reason,
            &evidence_artifact_id,
        ),
        _ => Ok(json!({
            "ignored": true,
            "reason": "failed work item has no router",
            "role": role,
            "target_entity_type": target_entity_type,
            "target_entity_id": target_entity_id,
            "failure_reason": failure_reason,
        })),
    }
}

fn route_failed_observer(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    signal_id: &str,
    failure_reason: &str,
    evidence_artifact_id: &str,
) -> Result<Value, String> {
    let signal = get_entity(ctx, base_url, headers, "Signals", signal_id)?;
    let status = entity_status(&signal);
    if matches!(status.as_str(), "Recorded" | "Linked") {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Signals",
            signal_id,
            "FailSignalObservation",
            json!({
                "error": "work_item_failed",
                "error_message": failure_reason,
                "integration": "work_item_result_router",
            }),
        )?;
    }
    link_evidence_if_present(
        ctx,
        base_url,
        headers,
        evidence_artifact_id,
        "Signal",
        signal_id,
    )?;
    Ok(json!({
        "routed": "observer_failure",
        "signal_id": signal_id,
        "failure_reason": failure_reason,
    }))
}

fn route_failed_variant_generator(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    work_item_id: &str,
    generation_id: &str,
    work_item_fields: &Value,
    failure_reason: &str,
    evidence_artifact_id: &str,
) -> Result<Value, String> {
    let generation = get_entity(ctx, base_url, headers, "Generations", generation_id)?;
    if matches!(entity_status(&generation).as_str(), "Completed" | "Failed") {
        return Ok(json!({
            "ignored": true,
            "reason": "generation already terminal",
            "generation_id": generation_id,
            "failure_reason": failure_reason,
        }));
    }

    let generation_fields = state_fields(&generation);
    let episode_id = field_str(&generation_fields, &["EpisodeId"]);
    let brain_run_id = field_str(work_item_fields, &["BrainRunId"]);
    let summary = format!("Variant generation failed: {failure_reason}");
    let app_ref = format!("failed://{work_item_id}");
    let branch_ref = format!(
        "directed-evolution/{generation_id}/failed-{}",
        short_id(work_item_id)
    );

    let variant_id = create_entity(ctx, base_url, headers, "Variants")?;
    let mutation_id = create_entity(ctx, base_url, headers, "Mutations")?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Mutations",
        &mutation_id,
        "RecordMutation",
        json!({
            "VariantId": variant_id,
            "Summary": summary,
            "ChangedFilesJson": "[]",
            "DiffRef": "",
            "BrainRunId": brain_run_id,
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Variants",
        &variant_id,
        "RecordVariantMutation",
        json!({
            "EpisodeId": episode_id,
            "GenerationId": generation_id,
            "MutationId": mutation_id,
            "AppRef": app_ref,
            "BranchRef": branch_ref,
            "Summary": summary,
            "BrainRunId": brain_run_id,
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Generations",
        generation_id,
        "RecordGeneratedVariant",
        json!({
            "VariantId": variant_id,
            "BrainRunId": brain_run_id,
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Variants",
        &variant_id,
        "FailVariant",
        json!({
            "FailureReason": failure_reason,
            "EvidenceArtifactId": evidence_artifact_id,
        }),
    )?;
    link_evidence_if_present(
        ctx,
        base_url,
        headers,
        evidence_artifact_id,
        "Variant",
        &variant_id,
    )?;
    let selection_work_item_id =
        maybe_finish_generation_after_evaluation(ctx, base_url, headers, generation_id)?;

    Ok(json!({
        "routed": "variant_generator_failure",
        "generation_id": generation_id,
        "variant_id": variant_id,
        "mutation_id": mutation_id,
        "selection_work_item_id": selection_work_item_id,
        "failure_reason": failure_reason,
    }))
}

fn route_failed_stage_result(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    work_item_id: &str,
    role: &str,
    stage_result_id: &str,
    failure_reason: &str,
    evidence_artifact_id: &str,
) -> Result<Value, String> {
    let stage_result = get_entity(ctx, base_url, headers, "StageResults", stage_result_id)?;
    let stage_result_fields = state_fields(&stage_result);
    let generation_id = field_str(&stage_result_fields, &["GenerationId"]);
    let variant_id = field_str(&stage_result_fields, &["VariantId"]);
    let stage_status = entity_status(&stage_result);

    if stage_status == "Running" {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "StageResults",
            stage_result_id,
            "FailStageResult",
            json!({
                "MetricsJson": "{}",
                "EvidenceArtifactId": evidence_artifact_id,
                "FailureReason": failure_reason,
            }),
        )?;
        post_directed_action(
            ctx,
            base_url,
            headers,
            "StageResults",
            stage_result_id,
            "EliminateStageResult",
            json!({
                "EliminationRuleId": "",
                "EvidenceArtifactId": evidence_artifact_id,
                "Reason": failure_reason,
            }),
        )?;
    } else if stage_status == "Failed" {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "StageResults",
            stage_result_id,
            "EliminateStageResult",
            json!({
                "EliminationRuleId": "",
                "EvidenceArtifactId": evidence_artifact_id,
                "Reason": failure_reason,
            }),
        )?;
    }

    if !variant_id.is_empty() {
        let variant = get_entity(ctx, base_url, headers, "Variants", &variant_id)?;
        if matches!(
            entity_status(&variant).as_str(),
            "Created" | "Building" | "Active"
        ) {
            post_directed_action(
                ctx,
                base_url,
                headers,
                "Variants",
                &variant_id,
                "EliminateVariant",
                json!({
                    "EliminationRuleId": "",
                    "StageResultId": stage_result_id,
                    "EvidenceArtifactId": evidence_artifact_id,
                    "Reason": failure_reason,
                }),
            )?;
        }
    }

    maybe_finish_trial_for_stage_result(
        ctx,
        base_url,
        headers,
        role,
        work_item_id,
        false,
        failure_reason,
        evidence_artifact_id,
        "{}",
    )?;
    link_evidence_if_present(
        ctx,
        base_url,
        headers,
        evidence_artifact_id,
        "StageResult",
        stage_result_id,
    )?;
    let selection_work_item_id = if generation_id.is_empty() {
        None
    } else {
        maybe_finish_generation_after_evaluation(ctx, base_url, headers, &generation_id)?
    };

    Ok(json!({
        "routed": "stage_result_failure",
        "stage_result_id": stage_result_id,
        "variant_id": variant_id,
        "selection_work_item_id": selection_work_item_id,
        "failure_reason": failure_reason,
    }))
}

fn route_failed_selector(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    generation_id: &str,
    failure_reason: &str,
    evidence_artifact_id: &str,
) -> Result<Value, String> {
    let generation = get_entity(ctx, base_url, headers, "Generations", generation_id)?;
    if matches!(entity_status(&generation).as_str(), "Completed" | "Failed") {
        return Ok(json!({
            "ignored": true,
            "reason": "generation already terminal",
            "generation_id": generation_id,
        }));
    }

    post_directed_action(
        ctx,
        base_url,
        headers,
        "Generations",
        generation_id,
        "FailGeneration",
        json!({ "FailureReason": failure_reason }),
    )?;
    let generation_fields = state_fields(&generation);
    let episode_id = field_str(&generation_fields, &["EpisodeId"]);
    if !episode_id.is_empty() {
        let episode = get_entity(ctx, base_url, headers, "Episodes", &episode_id)?;
        maybe_fail_episode(ctx, base_url, headers, &episode, &episode_id, failure_reason)?;
    }
    link_evidence_if_present(
        ctx,
        base_url,
        headers,
        evidence_artifact_id,
        "Generation",
        generation_id,
    )?;

    Ok(json!({
        "routed": "selector_failure",
        "generation_id": generation_id,
        "episode_id": episode_id,
        "failure_reason": failure_reason,
    }))
}

fn route_failed_promoter(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    promotion_id: &str,
    failure_reason: &str,
    evidence_artifact_id: &str,
) -> Result<Value, String> {
    let promotion = get_entity(ctx, base_url, headers, "Promotions", promotion_id)?;
    if entity_status(&promotion) == "Promoted" {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Promotions",
            promotion_id,
            "RecordPromotionMaterializationFailure",
            json!({
                "FailureReason": failure_reason,
                "EvidenceArtifactId": evidence_artifact_id,
            }),
        )?;
    }
    link_evidence_if_present(
        ctx,
        base_url,
        headers,
        evidence_artifact_id,
        "Promotion",
        promotion_id,
    )?;

    Ok(json!({
        "routed": "promoter_failure",
        "promotion_id": promotion_id,
        "failure_reason": failure_reason,
    }))
}

fn link_evidence_if_present(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    evidence_artifact_id: &str,
    target_entity_type: &str,
    target_entity_id: &str,
) -> Result<(), String> {
    if evidence_artifact_id.trim().is_empty() {
        return Ok(());
    }
    link_evidence(
        ctx,
        base_url,
        headers,
        evidence_artifact_id,
        target_entity_type,
        target_entity_id,
    )
}
