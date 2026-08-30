#[derive(Clone)]
struct VariantOutcome {
    id: String,
    status: String,
    app_ref: String,
    branch_ref: String,
    summary: String,
    evidence_summary: String,
    complete: bool,
    survived: bool,
}

struct RouterEnv<'a> {
    ctx: &'a Context,
    base_url: &'a str,
    headers: &'a [(String, String)],
}

struct FollowupGenerationInput<'a> {
    generation_fields: &'a Value,
    episode: &'a Value,
    episode_fields: &'a Value,
    previous_generation_id: &'a str,
    episode_id: &'a str,
    outcomes: &'a [VariantOutcome],
    target_count: usize,
}

struct FollowupPromptInput<'a> {
    episode_id: &'a str,
    generation_id: &'a str,
    previous_generation_id: &'a str,
    organism_id: &'a str,
    direction_id: &'a str,
    parent_version_id: &'a str,
    variant_index: usize,
    variant_target_count: usize,
    prompt_context: &'a str,
}

struct GenerationFailureInput<'a> {
    generation: &'a Value,
    generation_id: &'a str,
    episode: &'a Value,
    episode_id: &'a str,
    reason: &'a str,
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
        let env = RouterEnv {
            ctx,
            base_url,
            headers,
        };
        if let Some(next_generation_id) = queue_followup_generation_if_allowed(
            &env,
            FollowupGenerationInput {
                generation_fields: &generation_fields,
                episode: &episode,
                episode_fields: &episode_fields,
                previous_generation_id: generation_id,
                episode_id: &episode_id,
                outcomes: &outcomes,
                target_count,
            },
        )? {
            return Ok(Some(next_generation_id));
        }
        fail_generation_and_episode(
            &env,
            GenerationFailureInput {
                generation: &generation,
                generation_id,
                episode: &episode,
                episode_id: &episode_id,
                reason: "All variants were eliminated before selection.",
            },
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
        let evidence_summary = stage_results
            .iter()
            .filter_map(stage_result_evidence_summary)
            .collect::<Vec<_>>()
            .join(" | ");
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
            evidence_summary,
            complete,
            survived,
        });
    }
    Ok(outcomes)
}

fn stage_result_evidence_summary(result: &Value) -> Option<String> {
    let status = entity_status(result);
    let fields = state_fields(result);
    let summary = nonempty(
        field_str(&fields, &["Reason"]),
        nonempty(
            field_str(&fields, &["FailureReason"]),
            field_str(&fields, &["Summary"]),
        ),
    );
    if summary.trim().is_empty() {
        None
    } else {
        Some(format!("{status}: {summary}"))
    }
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

fn queue_followup_generation_if_allowed(
    env: &RouterEnv<'_>,
    input: FollowupGenerationInput<'_>,
) -> Result<Option<String>, String> {
    if entity_status(input.episode) != "Running" {
        return Ok(None);
    }

    let current_index = field_u64(input.generation_fields, &["GenerationIndex"]) as usize;
    let max_generation_count = config_usize(env.ctx, "max_generation_count", 2);
    if current_index >= max_generation_count {
        return Ok(None);
    }

    let next_generation_index = current_index + 1;
    let next_generation_id = create_entity(env.ctx, env.base_url, env.headers, "Generations")?;
    let parent_version_id = field_str(input.generation_fields, &["ParentVersionId"]);
    let variant_target_count = if input.target_count > 0 {
        input.target_count
    } else {
        config_usize(env.ctx, "variant_target_count", 3)
    };

    post_directed_action(
        env.ctx,
        env.base_url,
        env.headers,
        "Generations",
        &next_generation_id,
        "StartGeneration",
        json!({
            "EpisodeId": input.episode_id,
            "ParentVersionId": parent_version_id,
            "GenerationIndex": next_generation_index.to_string(),
            "VariantTargetCount": variant_target_count.to_string(),
        }),
    )?;
    post_directed_action(
        env.ctx,
        env.base_url,
        env.headers,
        "Episodes",
        input.episode_id,
        "AddGeneration",
        json!({ "GenerationId": next_generation_id }),
    )?;

    let prompt_context = followup_generation_context(
        env.ctx,
        env.base_url,
        env.headers,
        input.episode_fields,
        input.previous_generation_id,
        input.outcomes,
    );
    let organism_id = field_str(input.episode_fields, &["OrganismId"]);
    let direction_id = field_str(input.episode_fields, &["DirectionId"]);
    for variant_index in 1..=variant_target_count {
        let work_item_id = create_entity(env.ctx, env.base_url, env.headers, "WorkItems")?;
        let prompt = followup_variant_generator_prompt(FollowupPromptInput {
            episode_id: input.episode_id,
            generation_id: &next_generation_id,
            previous_generation_id: input.previous_generation_id,
            organism_id: &organism_id,
            direction_id: &direction_id,
            parent_version_id: &parent_version_id,
            variant_index,
            variant_target_count,
            prompt_context: &prompt_context,
        });
        post_directed_action(
            env.ctx,
            env.base_url,
            env.headers,
            "WorkItems",
            &work_item_id,
            "QueueWorkItem",
            json!({
                "Role": "variant_generator",
                "TargetEntityType": "Generation",
                "TargetEntityId": next_generation_id,
                "PromptRef": format!("literal:{prompt}"),
                "ContextRef": format!("episode:{}", input.episode_id),
                "OutputSchemaRef": "directed-evolution.variant-generator.v1",
                "CorrelationJson": json!({
                    "episode_id": input.episode_id,
                    "generation_id": next_generation_id,
                    "previous_generation_id": input.previous_generation_id,
                    "organism_id": organism_id,
                    "direction_id": direction_id,
                    "variant_index": variant_index,
                    "variant_target_count": variant_target_count,
                }).to_string(),
            }),
        )?;
    }

    post_directed_action(
        env.ctx,
        env.base_url,
        env.headers,
        "Generations",
        input.previous_generation_id,
        "FailGeneration",
        json!({
            "FailureReason": format!(
                "All variants were eliminated before selection. Queued follow-up generation {} with prior elimination evidence.",
                next_generation_index
            ),
        }),
    )?;

    Ok(Some(next_generation_id))
}

fn followup_generation_context(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode_fields: &Value,
    previous_generation_id: &str,
    outcomes: &[VariantOutcome],
) -> String {
    let mut sections = Vec::new();
    let parent_version_id = field_str(episode_fields, &["ParentVersionId"]);
    let parent = entity_fields_or_empty(
        ctx,
        base_url,
        headers,
        "OrganismVersions",
        &parent_version_id,
    );
    sections.push(format!(
        "Parent AppRef: {}\nParent Summary: {}",
        field_str(&parent, &["AppRef"]),
        field_str(&parent, &["Summary"])
    ));

    let direction_id = field_str(episode_fields, &["DirectionId"]);
    let direction = entity_fields_or_empty(ctx, base_url, headers, "Directions", &direction_id);
    sections.push(format!(
        "Direction: {}\nDirection Summary: {}\nDirection Pressure Class: {}\nProposed Adaptation Goal: {}",
        field_str(&direction, &["Title"]),
        field_str(&direction, &["Summary"]),
        field_str(&direction, &["PressureClass"]),
        field_str(&direction, &["ProposedAdaptationGoal"])
    ));

    let adaptation_goal_id = field_str(episode_fields, &["AdaptationGoalId"]);
    let adaptation_goal = entity_fields_or_empty(
        ctx,
        base_url,
        headers,
        "AdaptationGoals",
        &adaptation_goal_id,
    );
    if !adaptation_goal_id.is_empty() {
        sections.push(format!(
            "Adaptation Goal: {}",
            field_str(&adaptation_goal, &["GoalStatement"])
        ));
    }

    let selection_pressure_id = field_str(episode_fields, &["SelectionPressureId"]);
    let selection_pressure = entity_fields_or_empty(
        ctx,
        base_url,
        headers,
        "SelectionPressures",
        &selection_pressure_id,
    );
    if !selection_pressure_id.is_empty() {
        sections.push(format!(
            "Selection Pressure: {}",
            field_str(&selection_pressure, &["SelectionStatement"])
        ));
    }

    let constraints =
        parse_json_string_array(&field_str(episode_fields, &["ViabilityConstraintIdsJson"]))
            .into_iter()
            .filter_map(|constraint_id| {
                let fields = entity_fields_or_empty(
                    ctx,
                    base_url,
                    headers,
                    "ViabilityConstraints",
                    &constraint_id,
                );
                let statement = field_str(&fields, &["ConstraintStatement"]);
                (!statement.is_empty()).then_some(format!(
                    "- {} ({})",
                    statement,
                    field_str(&fields, &["ConstraintKind"])
                ))
            })
            .collect::<Vec<_>>();
    if !constraints.is_empty() {
        sections.push(format!(
            "Viability Constraints:\n{}",
            constraints.join("\n")
        ));
    }

    sections.push(format!(
        "Previous Generation Evidence ({previous_generation_id}):\n{}",
        eliminated_generation_evidence_context(outcomes)
    ));
    sections.push(
        "Use the prior evidence as selection pressure for this generation. Do not repeat a mutation family that only changes metadata if runtime evidence showed the underlying state field still does not resolve."
            .to_string(),
    );
    sections
        .push("Do not modify the evaluator, viability constraints, or selection pressure.".to_string());

    sections.join("\n\n")
}

fn eliminated_generation_evidence_context(outcomes: &[VariantOutcome]) -> String {
    let lines = outcomes
        .iter()
        .filter(|outcome| !outcome.survived)
        .map(|outcome| {
            format!(
                "- Variant {} [{}]: {} Evidence: {}",
                outcome.id,
                outcome.status,
                compact(&outcome.summary, 220),
                compact(&outcome.evidence_summary, 420)
            )
        })
        .collect::<Vec<_>>();
    if lines.is_empty() {
        "- No eliminated variants recorded.".to_string()
    } else {
        lines.join("\n")
    }
}

fn followup_variant_generator_prompt(input: FollowupPromptInput<'_>) -> String {
    format!(
        "Generate Directed Evolution follow-up variant {} of {}.\n\
EpisodeId: {}\n\
GenerationId: {}\n\
PreviousGenerationId: {}\n\
OrganismId: {}\n\
DirectionId: {}\n\
ParentVersionId: {}\n\n\
{}\n\n\
Variant lane suggestion: {}\n\
Work in the assigned organism repository and create one real candidate variant. \
This is a follow-up generation, so explicitly address the concrete elimination evidence \
from the previous generation instead of restating the same metadata-only repair. \
Keep the mutation bounded to the Agent Answers app bundle: prefer changing APP.md, \
adrs/, specs/question.ioa.toml, specs/answer.ioa.toml, specs/model.csdl.xml, and \
policies/agent_answers.cedar. Preserve existing Question and Answer actions. \
For repair episodes, repair the observed failure directly and do not add product-growth \
features, intent-capture affordances, or optional metadata unless they are required by \
the repair and automatically maintained by the existing lifecycle. \
Return JSON with: summary, app_ref, branch_ref, runtime_ref, changed_files, diff_ref, \
verification_notes, and next_actions. Do not change evaluation rules or viability constraints.",
        input.variant_index,
        input.variant_target_count,
        input.episode_id,
        input.generation_id,
        input.previous_generation_id,
        input.organism_id,
        input.direction_id,
        input.parent_version_id,
        input.prompt_context,
        followup_variant_lane_suggestion(input.variant_index),
    )
}

fn followup_variant_lane_suggestion(variant_index: usize) -> &'static str {
    match variant_index {
        1 => {
            "Repair the state/spec mismatch named by the prior evidence. Prefer aligning persisted IOA field names with CSDL referential constraints over adding a parallel navigation-only declaration."
        }
        2 => {
            "Repair the runtime projection path so the existing submitted answer can be resolved from its question without a new manual action or product feature."
        }
        3 => {
            "Repair with executable regression evidence that Configure -> Submit -> RecordAnswer makes Question.Answers and Answer.Question resolve before Accept is evaluated."
        }
        _ => {
            "Make the smallest backward-compatible repair that directly addresses the previous generation's concrete elimination reason."
        }
    }
}

fn fail_generation_and_episode(
    env: &RouterEnv<'_>,
    input: GenerationFailureInput<'_>,
) -> Result<(), String> {
    if !matches!(entity_status(input.generation).as_str(), "Completed" | "Failed") {
        post_directed_action(
            env.ctx,
            env.base_url,
            env.headers,
            "Generations",
            input.generation_id,
            "FailGeneration",
            json!({
                "FailureReason": input.reason,
            }),
        )?;
    }
    maybe_fail_episode(
        env.ctx,
        env.base_url,
        env.headers,
        input.episode,
        input.episode_id,
        input.reason,
    )
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
