fn route_simulated_user_trial(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    _work_item_id: &str,
    trial_id: &str,
    work_item_fields: &Value,
    output: &Value,
) -> Result<Value, String> {
    let trial = get_entity(ctx, base_url, headers, "Trials", trial_id)?;
    let trial_fields = state_fields(&trial);
    let status = lookup_string_deep(output, &["status", "Status"]).to_ascii_lowercase();
    let blocker = lookup_string_deep(output, &["blocker", "Blocker"]);
    let blocker_kind = nonempty(
        lookup_string_deep(
            output,
            &[
                "blocker_kind",
                "blockerKind",
                "blocker_scope",
                "blockerScope",
                "BlockerKind",
            ],
        ),
        classify_blocker_kind(&blocker, &lookup_string_deep(output, &["summary", "reasoning_summary"])),
    );
    let is_blocked = status.contains("blocked") || !blocker.trim().is_empty();
    let evidence_artifact_id = field_str(work_item_fields, &["EvidenceArtifactId"]);
    let summary = nonempty(
        lookup_string_deep(output, &["summary", "reasoning_summary"]),
        field_str(work_item_fields, &["Summary"]),
    );
    let journey_json =
        lookup_value_deep(output, &["journey", "Journey"]).unwrap_or_else(|| json!([])).to_string();
    let observation_json = lookup_value_deep(output, &["observations", "Observations"])
        .unwrap_or_else(|| json!({}))
        .to_string();
    let intent_satisfied =
        lookup_string_deep(output, &["intent_satisfied", "intentSatisfied", "IntentSatisfied"]);
    let friction_json =
        lookup_value_deep(output, &["friction", "Friction"]).unwrap_or_else(|| json!([])).to_string();
    let measurements_json =
        lookup_value_deep(output, &["metrics", "Metrics"]).unwrap_or_else(|| json!({})).to_string();

    if is_blocked {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Trials",
            trial_id,
            "FailTrial",
            json!({
                "FailureReason": nonempty(blocker.clone(), summary.clone()),
                "EvidenceArtifactId": evidence_artifact_id,
                "MeasurementsJson": measurements_json,
                "JourneyJson": journey_json,
                "ObservationJson": observation_json,
                "IntentSatisfied": intent_satisfied,
                "FrictionJson": friction_json,
                "Blocker": blocker,
                "BlockerKind": blocker_kind,
            }),
        )?;
    } else {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Trials",
            trial_id,
            "SucceedTrial",
            json!({
                "Summary": summary,
                "EvidenceArtifactId": evidence_artifact_id,
                "MeasurementsJson": measurements_json,
                "JourneyJson": journey_json,
                "ObservationJson": observation_json,
                "IntentSatisfied": intent_satisfied,
                "FrictionJson": friction_json,
                "Blocker": blocker,
                "BlockerKind": blocker_kind,
            }),
        )?;
    }

    let queued_evaluator = maybe_queue_viability_evaluator_after_trials(
        ctx,
        base_url,
        headers,
        &trial_fields,
    )?;
    let queued_deferred_evaluators = maybe_queue_deferred_stage_evaluators_after_trials(
        ctx,
        base_url,
        headers,
        &trial_fields,
    )?;

    Ok(json!({
        "routed": "simulated_user_trial",
        "trial_id": trial_id,
        "blocked": is_blocked,
        "queued_evaluator_work_item_id": queued_evaluator,
        "queued_deferred_evaluator_work_item_ids": queued_deferred_evaluators,
    }))
}

fn classify_blocker_kind(blocker: &str, summary: &str) -> String {
    let haystack = format!("{blocker}\n{summary}").to_ascii_lowercase();
    if haystack.trim().is_empty() {
        return "none".to_string();
    }
    if haystack.contains("no live app route")
        || haystack.contains("no route")
        || haystack.contains("router-level")
        || haystack.contains("404")
        || haystack.contains("metadata")
        || haystack.contains("/tdata")
        || haystack.contains("runtime host")
        || haystack.contains("gateway")
        || haystack.contains("unreachable")
        || haystack.contains("not routable")
        || haystack.contains("route availability")
    {
        return "runtime-access".to_string();
    }
    if haystack.contains("missing")
        || haystack.contains("could not compare")
        || haystack.contains("could not inspect")
        || haystack.contains("not exposed")
        || haystack.contains("not visible")
        || haystack.contains("unsupported")
    {
        return "app-behavior".to_string();
    }
    "ambiguous".to_string()
}

fn maybe_queue_viability_evaluator_after_trials(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    trial_fields: &Value,
) -> Result<Option<String>, String> {
    let stage_result_id = field_str(trial_fields, &["StageResultId"]);
    if stage_result_id.trim().is_empty() {
        return Ok(None);
    }
    let trial_filter = format!(
        "StageResultId%20eq%20'{}'",
        escape_odata_id(&stage_result_id)
    );
    let trials = list_entities(ctx, base_url, headers, "Trials", &trial_filter)?;
    if trials.is_empty() {
        return Ok(None);
    }
    let all_terminal = trials
        .iter()
        .all(|trial| matches!(entity_status(trial).as_str(), "Succeeded" | "Failed" | "Archived"));
    if !all_terminal {
        return Ok(None);
    }
    let existing_filter = format!(
        "Role%20eq%20'viability_evaluator'%20and%20TargetEntityType%20eq%20'StageResult'%20and%20TargetEntityId%20eq%20'{}'",
        escape_odata_id(&stage_result_id)
    );
    for work_item in list_entities(ctx, base_url, headers, "WorkItems", &existing_filter)? {
        if matches!(
            entity_status(&work_item).as_str(),
            "Queued" | "Claimed" | "Running" | "Succeeded"
        ) {
            return Ok(Some(entity_id_from_entity(&work_item)));
        }
    }

    let stage_result = get_entity(ctx, base_url, headers, "StageResults", &stage_result_id)?;
    if entity_status(&stage_result) != "Pending" {
        return Ok(None);
    }
    let stage_result_fields = state_fields(&stage_result);
    let episode_id = field_str(&stage_result_fields, &["EpisodeId"]);
    let generation_id = field_str(&stage_result_fields, &["GenerationId"]);
    let variant_id = field_str(&stage_result_fields, &["VariantId"]);
    let stage_id = field_str(&stage_result_fields, &["EvaluationStageId"]);
    let episode = get_entity(ctx, base_url, headers, "Episodes", &episode_id)?;
    let episode_fields = state_fields(&episode);
    let stage = get_entity(ctx, base_url, headers, "EvaluationStages", &stage_id)?;
    let variant = get_entity(ctx, base_url, headers, "Variants", &variant_id)?;
    let variant_fields = state_fields(&variant);
    let role = "viability_evaluator";
    let work_item_id = create_entity(ctx, base_url, headers, "WorkItems")?;
    let prompt = evaluation_prompt(
        ctx,
        base_url,
        headers,
        &episode_fields,
        &stage,
        &variant_id,
        &generation_id,
        &episode_id,
        &stage_id,
        &stage_result_id,
        &work_item_id,
        &field_str(&variant_fields, &["Summary"]),
        &field_str(&variant_fields, &["AppRef"]),
        &field_str(&variant_fields, &["RuntimeRef"]),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "WorkItems",
        &work_item_id,
        "QueueWorkItem",
        json!({
            "Role": role,
            "TargetEntityType": "StageResult",
            "TargetEntityId": stage_result_id,
            "PromptRef": format!("literal:{prompt}"),
            "ContextRef": format!("stage_result:{stage_result_id}"),
            "OutputSchemaRef": "directed-evolution.stage-evaluation.v1",
            "CorrelationJson": json!({
                "episode_id": episode_id,
                "direction_id": field_str(&episode_fields, &["DirectionId"]),
                "generation_id": generation_id,
                "variant_id": variant_id,
                "evaluation_stage_id": stage_id,
                "stage_result_id": stage_result_id,
                "trial_count": trials.len(),
                "role": role,
            }).to_string(),
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "StageResults",
        &stage_result_id,
        "StartStageResult",
        json!({
            "EpisodeId": episode_id,
            "GenerationId": generation_id,
            "VariantId": variant_id,
            "EvaluationStageId": stage_id,
            "WorkItemId": work_item_id,
        }),
    )?;
    Ok(Some(work_item_id))
}

fn maybe_queue_deferred_stage_evaluators_after_trials(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    trial_fields: &Value,
) -> Result<Vec<String>, String> {
    let stage_result_id = field_str(trial_fields, &["StageResultId"]);
    if stage_result_id.trim().is_empty() {
        return Ok(Vec::new());
    }

    let trial_filter = format!(
        "StageResultId%20eq%20'{}'",
        escape_odata_id(&stage_result_id)
    );
    let trials = list_entities(ctx, base_url, headers, "Trials", &trial_filter)?;
    if trials.is_empty()
        || !trials.iter().all(|trial| {
            matches!(
                entity_status(trial).as_str(),
                "Succeeded" | "Failed" | "Archived"
            )
        })
    {
        return Ok(Vec::new());
    }

    let episode_id = field_str(trial_fields, &["EpisodeId"]);
    let generation_id = field_str(trial_fields, &["GenerationId"]);
    let variant_id = field_str(trial_fields, &["VariantId"]);
    if episode_id.trim().is_empty()
        || generation_id.trim().is_empty()
        || variant_id.trim().is_empty()
    {
        return Ok(Vec::new());
    }

    let episode = get_entity(ctx, base_url, headers, "Episodes", &episode_id)?;
    let episode_fields = state_fields(&episode);
    let generation = get_entity(ctx, base_url, headers, "Generations", &generation_id)?;
    let generation_fields = state_fields(&generation);
    let variant = get_entity(ctx, base_url, headers, "Variants", &variant_id)?;
    if entity_status(&variant) != "Active" {
        return Ok(Vec::new());
    }
    let variant_fields = state_fields(&variant);
    let stage_result_filter = format!(
        "EpisodeId%20eq%20'{}'%20and%20GenerationId%20eq%20'{}'%20and%20VariantId%20eq%20'{}'",
        escape_odata_id(&episode_id),
        escape_odata_id(&generation_id),
        escape_odata_id(&variant_id)
    );
    let mut queued = Vec::new();
    for stage_result in list_entities(ctx, base_url, headers, "StageResults", &stage_result_filter)? {
        if entity_status(&stage_result) != "Pending" {
            continue;
        }
        let fields = state_fields(&stage_result);
        let candidate_stage_result_id = entity_id_from_entity(&stage_result);
        let stage_id = field_str(&fields, &["EvaluationStageId"]);
        if stage_id.trim().is_empty() {
            continue;
        }
        let stage = get_entity(ctx, base_url, headers, "EvaluationStages", &stage_id)?;
        let stage_fields = state_fields(&stage);
        if !defer_stage_until_simulated_users(&stage_fields) {
            continue;
        }
        let role = evaluator_role_for_stage(&stage_fields);
        let work_item_id = queue_stage_evaluation_work_item(
            ctx,
            base_url,
            headers,
            &episode_fields,
            &stage,
            &variant_id,
            &generation_id,
            &episode_id,
            &stage_id,
            &candidate_stage_result_id,
            &field_str(&variant_fields, &["Summary"]),
            &field_str(&variant_fields, &["AppRef"]),
            &field_str(&variant_fields, &["RuntimeRef"]),
            &role,
            &field_str(&generation_fields, &["ParentVersionId"]),
        )?;
        post_directed_action(
            ctx,
            base_url,
            headers,
            "StageResults",
            &candidate_stage_result_id,
            "StartStageResult",
            json!({
                "EpisodeId": episode_id,
                "GenerationId": generation_id,
                "VariantId": variant_id,
                "EvaluationStageId": stage_id,
                "WorkItemId": work_item_id,
            }),
        )?;
        queued.push(work_item_id);
    }

    Ok(queued)
}
