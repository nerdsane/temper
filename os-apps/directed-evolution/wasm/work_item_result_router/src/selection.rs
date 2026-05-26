#[derive(Clone)]
struct VariantOutcome {
    id: String,
    status: String,
    app_ref: String,
    branch_ref: String,
    summary: String,
    complete: bool,
    survived: bool,
}

fn maybe_finish_generation_after_evaluation(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    generation_id: &str,
) -> Result<Option<String>, String> {
    let generation = get_entity(ctx, base_url, headers, "Generations", generation_id)?;
    let generation_status = entity_status(&generation);
    if matches!(generation_status.as_str(), "Completed" | "Failed") {
        return Ok(None);
    }

    let generation_fields = state_fields(&generation);
    let episode_id = field_str(&generation_fields, &["EpisodeId"]);
    let target_count = field_u64(&generation_fields, &["VariantTargetCount"]) as usize;
    let episode = get_entity(ctx, base_url, headers, "Episodes", &episode_id)?;
    let episode_fields = state_fields(&episode);
    let stage_ids =
        parse_json_string_array(&field_str(&episode_fields, &["EvaluationStageIdsJson"]));
    let variants = list_variants_for_generation(ctx, base_url, headers, generation_id)?;
    if target_count > 0 && variants.len() < target_count {
        return Ok(None);
    }

    let outcomes =
        collect_generation_outcomes(ctx, base_url, headers, generation_id, stage_ids.len())?;
    if outcomes.iter().any(|outcome| !outcome.complete) {
        return Ok(None);
    }

    let survivors = outcomes
        .iter()
        .filter(|outcome| outcome.survived)
        .map(|outcome| outcome.id.clone())
        .collect::<Vec<_>>();
    if survivors.is_empty() {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Generations",
            generation_id,
            "FailGeneration",
            json!({
                "FailureReason": "All variants were eliminated before selection.",
            }),
        )?;
        maybe_fail_episode(
            ctx,
            base_url,
            headers,
            &episode,
            &episode_id,
            "All variants were eliminated before selection.",
        )?;
        return Ok(Some("generation_failed".to_string()));
    }

    ensure_episode_selection_started(ctx, base_url, headers, &episode, &episode_id, generation_id)?;
    ensure_generation_selection_started(ctx, base_url, headers, &generation, generation_id)?;
    queue_selector_if_absent(
        ctx,
        base_url,
        headers,
        generation_id,
        &episode_id,
        &survivors,
        outcomes.len(),
    )
}

fn collect_generation_outcomes(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    generation_id: &str,
    stage_count: usize,
) -> Result<Vec<VariantOutcome>, String> {
    let variants = list_variants_for_generation(ctx, base_url, headers, generation_id)?;
    let mut outcomes = Vec::with_capacity(variants.len());
    for variant in variants {
        let id = entity_id_from_entity(&variant);
        let status = entity_status(&variant);
        let fields = state_fields(&variant);
        let stage_results = list_stage_results_for_variant(ctx, base_url, headers, &id)?;
        let passed_count = stage_results
            .iter()
            .filter(|result| entity_status(result) == "Passed")
            .count();
        let has_failed_stage = stage_results
            .iter()
            .any(|result| matches!(entity_status(result).as_str(), "Failed" | "Eliminated"));
        let has_pending_stage = stage_results
            .iter()
            .any(|result| matches!(entity_status(result).as_str(), "Pending" | "Running"));
        let eliminated = matches!(status.as_str(), "Eliminated" | "Failed");
        let promoted_or_selected = matches!(status.as_str(), "Selected" | "Promoted");
        let survived = !eliminated
            && !has_failed_stage
            && !has_pending_stage
            && (stage_count == 0 || passed_count >= stage_count);
        let complete = eliminated || promoted_or_selected || survived;
        outcomes.push(VariantOutcome {
            id,
            status,
            app_ref: field_str(&fields, &["AppRef"]),
            branch_ref: field_str(&fields, &["BranchRef"]),
            summary: field_str(&fields, &["Summary"]),
            complete,
            survived,
        });
    }
    Ok(outcomes)
}

fn list_variants_for_generation(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    generation_id: &str,
) -> Result<Vec<Value>, String> {
    let filter = format!("GenerationId%20eq%20'{}'", escape_odata_id(generation_id));
    list_entities(ctx, base_url, headers, "Variants", &filter)
}

fn list_stage_results_for_variant(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    variant_id: &str,
) -> Result<Vec<Value>, String> {
    let filter = format!("VariantId%20eq%20'{}'", escape_odata_id(variant_id));
    list_entities(ctx, base_url, headers, "StageResults", &filter)
}

fn queue_selector_if_absent(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    generation_id: &str,
    episode_id: &str,
    survivor_ids: &[String],
    variant_count: usize,
) -> Result<Option<String>, String> {
    let filter = format!(
        "Role%20eq%20'selector'%20and%20TargetEntityType%20eq%20'Generation'%20and%20TargetEntityId%20eq%20'{}'",
        escape_odata_id(generation_id)
    );
    let existing = list_entities(ctx, base_url, headers, "WorkItems", &filter)?;
    for work_item in existing {
        let status = entity_status(&work_item);
        if matches!(
            status.as_str(),
            "Queued" | "Claimed" | "Running" | "Succeeded"
        ) {
            return Ok(Some(entity_id_from_entity(&work_item)));
        }
    }

    let work_item_id = create_entity(ctx, base_url, headers, "WorkItems")?;
    let prompt = selector_prompt(
        ctx,
        base_url,
        headers,
        generation_id,
        episode_id,
        survivor_ids,
        variant_count,
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "WorkItems",
        &work_item_id,
        "QueueWorkItem",
        json!({
            "Role": "selector",
            "TargetEntityType": "Generation",
            "TargetEntityId": generation_id,
            "PromptRef": format!("literal:{prompt}"),
            "ContextRef": format!("generation:{generation_id}"),
            "OutputSchemaRef": "directed-evolution.selector.v1",
            "CorrelationJson": json!({
                "episode_id": episode_id,
                "generation_id": generation_id,
                "survivor_ids": survivor_ids,
                "variant_count": variant_count,
            }).to_string(),
        }),
    )?;
    Ok(Some(work_item_id))
}

fn ensure_generation_selection_started(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    generation: &Value,
    generation_id: &str,
) -> Result<(), String> {
    if entity_status(generation) == "Evaluating" {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Generations",
            generation_id,
            "BeginGenerationSelection",
            json!({
                "Reason": "All generated variants reached an evaluation terminal state.",
            }),
        )?;
    }
    Ok(())
}

fn ensure_episode_selection_started(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode: &Value,
    episode_id: &str,
    generation_id: &str,
) -> Result<(), String> {
    if entity_status(episode) == "Running" {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Episodes",
            episode_id,
            "BeginEpisodeSelection",
            json!({
                "GenerationId": generation_id,
                "Reason": "All generated variants reached an evaluation terminal state.",
            }),
        )?;
    }
    Ok(())
}

fn maybe_fail_episode(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode: &Value,
    episode_id: &str,
    reason: &str,
) -> Result<(), String> {
    if matches!(
        entity_status(episode).as_str(),
        "Draft" | "Negotiating" | "Running" | "Paused" | "Selecting" | "Promoting"
    ) {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Episodes",
            episode_id,
            "FailEpisode",
            json!({ "FailureReason": reason }),
        )?;
    }
    Ok(())
}

fn link_evidence(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    evidence_artifact_id: &str,
    target_entity_type: &str,
    target_entity_id: &str,
) -> Result<(), String> {
    post_directed_action(
        ctx,
        base_url,
        headers,
        "EvidenceArtifacts",
        evidence_artifact_id,
        "LinkEvidenceArtifact",
        json!({
            "TargetEntityType": target_entity_type,
            "TargetEntityId": target_entity_id,
        }),
    )?;
    Ok(())
}
