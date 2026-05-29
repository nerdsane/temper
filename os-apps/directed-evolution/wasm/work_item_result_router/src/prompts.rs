fn evaluation_prompt(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode_fields: &Value,
    stage: &Value,
    variant_id: &str,
    generation_id: &str,
    episode_id: &str,
    stage_id: &str,
    stage_result_id: &str,
    work_item_id: &str,
    variant_summary: &str,
    app_ref: &str,
    runtime_ref_override: &str,
) -> Result<String, String> {
    let stage_fields = state_fields(stage);
    let contract_context = episode_contract_context(ctx, base_url, headers, episode_fields)?;
    let variant_fields = entity_fields_or_empty(ctx, base_url, headers, "Variants", variant_id);
    let runtime_ref = nonempty(
        runtime_ref_override.to_string(),
        field_str(&variant_fields, &["RuntimeRef"]),
    );
    let trial_context =
        trial_evidence_context(ctx, base_url, headers, stage_result_id).unwrap_or_default();
    let prompt_api_base = resolve_public_api_url(ctx);
    let direction_id = field_str(episode_fields, &["DirectionId"]);
    Ok(format_evaluation_prompt(
        &stage_fields,
        &contract_context,
        &trial_context,
        &prompt_api_base,
        variant_id,
        generation_id,
        episode_id,
        &direction_id,
        stage_id,
        stage_result_id,
        work_item_id,
        variant_summary,
        app_ref,
        &runtime_ref,
    ))
}

fn format_evaluation_prompt(
    stage_fields: &Value,
    contract_context: &str,
    trial_context: &str,
    temper_api_base: &str,
    variant_id: &str,
    generation_id: &str,
    episode_id: &str,
    direction_id: &str,
    stage_id: &str,
    stage_result_id: &str,
    work_item_id: &str,
    variant_summary: &str,
    app_ref: &str,
    runtime_ref: &str,
) -> String {
    let stage_guidance = evaluation_stage_guidance(stage_fields);
    format!(
        "Evaluate Directed Evolution variant.\n\
EpisodeId: {episode_id}\n\
DirectionId: {direction_id}\n\
GenerationId: {generation_id}\n\
VariantId: {variant_id}\n\
EvaluationStageId: {stage_id}\n\
StageResultId: {stage_result_id}\n\
StageName: {}\n\
StageKind: {}\n\
RequiredEvidence: {}\n\
TemperApiBase: {temper_api_base}\n\
AppRef: {app_ref}\n\
RuntimeRef: {runtime_ref}\n\
VariantSummary: {variant_summary}\n\n\
EpisodeContract:\n{contract_context}\n\n\
RecordedTrialEvidence:\n{}\n\n\
DirectedEvolutionHeaders:\n{}\n\n\
Use the stage contract, Adaptation Goal, Viability Constraints, Selection Pressure, and real evidence. \
If RuntimeRef is a temper://tenant/<tenant>/app/<app_ref> value, exercise that live tenant through \
TemperApiBase /tdata OData calls with x-tenant-id set to the tenant from RuntimeRef. \
{stage_guidance} \
Return JSON with: passed, status, summary, metrics, evidence_refs, failure_reason, and next_actions. \
Do not modify evaluators or selection rules.",
        field_str(&stage_fields, &["StageName"]),
        field_str(&stage_fields, &["StageKind"]),
        compact(&field_str(&stage_fields, &["RequiredEvidenceJson"]), 1000),
        if trial_context.trim().is_empty() {
            "No trial evidence recorded for this stage result yet.".to_string()
        } else {
            trial_context.to_string()
        },
        directed_evolution_header_block(
            direction_id,
            episode_id,
            generation_id,
            variant_id,
            stage_id,
            stage_result_id,
            work_item_id,
            "",
            "",
            "",
            "",
            runtime_ref,
            app_ref,
        ),
    )
}

fn evaluation_stage_guidance(stage_fields: &Value) -> &'static str {
    if stage_requires_datadog(stage_fields) {
        "This is a Datadog-measured telemetry stage. Query Datadog for the exact DirectedEvolutionHeaders join fields using service:temper-platform \"directed evolution runtime request\" scoped by directed_evolution.episode_id, directed_evolution.variant_id, and the runtime tenant from RuntimeRef. Prefer Datadog MCP aggregate/SQL evidence over brittle log-explorer field syntax: use a broad runtime-request filter, select the directed_evolution episode_id and variant_id columns plus tenant, and count rows for this exact episode, variant, and tenant. Do not require directed_evolution.runtime_ref unless Datadog field discovery proves it is indexed for these logs. Return top-level provenance_kind=datadog-measured and make the first evidence_scope item the Datadog logs query or aggregate with query, time_window, result_count, interpretation, zero_result_meaning, and datadog_url. Zero matching runtime-request logs is failure; runtime OData probes are supporting evidence only and must not replace Datadog."
    } else {
        "This is not a Datadog telemetry stage. Do not fail this stage because Datadog runtime-request logs are absent, and do not return provenance_kind=datadog-measured unless the RequiredEvidence or StageKind explicitly requires Datadog. Use the diff, specs, state, recorded observations, and runtime probes appropriate to this non-telemetry stage."
    }
}

fn simulated_user_prompt(
    stage_fields: &Value,
    episode_id: &str,
    direction_id: &str,
    generation_id: &str,
    variant_id: &str,
    stage_id: &str,
    stage_result_id: &str,
    trial_id: &str,
    work_item_id: &str,
    simulated_user_id: &str,
    persona_index: usize,
    run_index: usize,
    persona: &Value,
    goal: &str,
    runtime_ref: &str,
    app_ref: &str,
    variant_summary: &str,
    temper_api_base: &str,
) -> String {
    format!(
        "Act as an AI simulated user for a Directed Evolution trial.\n\
EpisodeId: {episode_id}\n\
DirectionId: {direction_id}\n\
GenerationId: {generation_id}\n\
VariantId: {variant_id}\n\
EvaluationStageId: {stage_id}\n\
StageResultId: {stage_result_id}\n\
TrialId: {trial_id}\n\
SimulatedUserId: {simulated_user_id}\n\
RunIndex: {run_index}\n\
StageName: {}\n\
StageKind: {}\n\
TemperApiBase: {temper_api_base}\n\
RuntimeRef: {runtime_ref}\n\
Persona: {}\n\
Goal: {goal}\n\
VariantSummary: {variant_summary}\n\n\
DirectedEvolutionHeaders:\n{}\n\n\
Use the live app only. If RuntimeRef is a temper://tenant/<tenant>/app/<app_ref> value, send \
x-tenant-id for that tenant and include the DirectedEvolutionHeaders on every app/runtime request. \
Start runtime probing at /tdata and /tdata/$metadata; a 404 from / or decorative app routes is not \
a blocker when the OData runtime works. Only use status=blocked when /tdata and /tdata/$metadata are \
unreachable for the parsed runtime tenant, or when the live app behavior prevents the user journey. \
Do not judge viability, do not pass/fail the variant, do not score, and do not select a winner. \
Return JSON with status=observed|blocked, summary, journey, observations, intent_satisfied, friction, metrics, evidence_scope, evidence_refs, blocker, blocker_kind, and reasoning_summary. \
Use blocker_kind=none|runtime-access|app-behavior|ambiguous.",
        field_str(stage_fields, &["StageName"]),
        field_str(stage_fields, &["StageKind"]),
        persona,
        directed_evolution_header_block(
            direction_id,
            episode_id,
            generation_id,
            variant_id,
            stage_id,
            stage_result_id,
            trial_id,
            work_item_id,
            simulated_user_id,
            &persona_index.to_string(),
            &run_index.to_string(),
            runtime_ref,
            app_ref,
        ),
    )
}

fn directed_evolution_header_block(
    direction_id: &str,
    episode_id: &str,
    generation_id: &str,
    variant_id: &str,
    stage_id: &str,
    stage_result_id: &str,
    trial_id: &str,
    work_item_id: &str,
    simulated_user_id: &str,
    persona_index: &str,
    run_index: &str,
    runtime_ref: &str,
    app_ref: &str,
) -> String {
    [
        ("x-de-direction-id", direction_id),
        ("x-de-episode-id", episode_id),
        ("x-de-generation-id", generation_id),
        ("x-de-variant-id", variant_id),
        ("x-de-stage-id", stage_id),
        ("x-de-stage-result-id", stage_result_id),
        ("x-de-trial-id", trial_id),
        ("x-de-work-item-id", work_item_id),
        ("x-de-persona-index", persona_index),
        ("x-de-run-index", run_index),
        ("x-de-simulated-user-id", simulated_user_id),
        ("x-de-runtime-ref", runtime_ref),
        ("x-de-app-ref", app_ref),
    ]
    .into_iter()
    .filter(|(_, value)| !value.trim().is_empty())
    .map(|(name, value)| format!("{name}: {value}"))
    .collect::<Vec<_>>()
    .join("\n")
}

fn selector_prompt(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    generation_id: &str,
    episode_id: &str,
    survivor_ids: &[String],
    variant_count: usize,
) -> Result<String, String> {
    let episode = get_entity(ctx, base_url, headers, "Episodes", episode_id)?;
    let episode_fields = state_fields(&episode);
    let contract_context = episode_contract_context(ctx, base_url, headers, &episode_fields)?;
    let evidence_context = generation_evidence_context(ctx, base_url, headers, generation_id)?;
    Ok(format!(
        "Select the Directed Evolution winner for GenerationId: {generation_id}.\n\
Survivors: {}\n\
TotalVariants: {variant_count}\n\n\
EpisodeContract:\n{contract_context}\n\n\
VariantEvidence:\n{evidence_context}\n\n\
Use the Adaptation Goal, Viability Constraints, Selection Pressure, stage results, metrics, \
and evidence. Return JSON with: winning_variant_id, selection_explanation, app_ref, commit_ref, \
evidence_uri, digest, and tradeoffs. Do not modify evaluators or selection rules.",
        survivor_ids.join(",")
    ))
}

fn episode_contract_context(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode_fields: &Value,
) -> Result<String, String> {
    let mut lines = Vec::new();
    let adaptation_goal_id = field_str(episode_fields, &["AdaptationGoalId"]);
    if !adaptation_goal_id.trim().is_empty() {
        let goal = get_entity(
            ctx,
            base_url,
            headers,
            "AdaptationGoals",
            &adaptation_goal_id,
        )?;
        let fields = state_fields(&goal);
        lines.push(format!(
            "AdaptationGoal {adaptation_goal_id}: {}",
            compact(&field_str(&fields, &["GoalStatement"]), 1200)
        ));
        let human_notes = field_str(&fields, &["HumanNotes"]);
        if !human_notes.trim().is_empty() {
            lines.push(format!("HumanNotes: {}", compact(&human_notes, 800)));
        }
    }
    let selection_pressure_id = field_str(episode_fields, &["SelectionPressureId"]);
    if !selection_pressure_id.trim().is_empty() {
        let pressure = get_entity(
            ctx,
            base_url,
            headers,
            "SelectionPressures",
            &selection_pressure_id,
        )?;
        let fields = state_fields(&pressure);
        lines.push(format!(
            "SelectionPressure {selection_pressure_id}: {}",
            compact(&field_str(&fields, &["SelectionStatement"]), 1200)
        ));
        for key in [
            "MetricIdsJson",
            "EliminationRuleIdsJson",
            "ScoringRuleIdsJson",
        ] {
            let value = field_str(&fields, &[key]);
            if !value.trim().is_empty() {
                lines.push(format!("{key}: {}", compact(&value, 800)));
            }
        }
    }
    for constraint_id in
        parse_json_string_array(&field_str(episode_fields, &["ViabilityConstraintIdsJson"]))
    {
        let constraint = get_entity(
            ctx,
            base_url,
            headers,
            "ViabilityConstraints",
            &constraint_id,
        )?;
        let fields = state_fields(&constraint);
        lines.push(format!(
            "ViabilityConstraint {constraint_id} [{}]: {}",
            field_str(&fields, &["ConstraintKind"]),
            compact(&field_str(&fields, &["ConstraintStatement"]), 1200)
        ));
    }
    if lines.is_empty() {
        lines.push("No episode contract entities were linked yet.".to_string());
    }
    Ok(lines.join("\n"))
}

fn generation_evidence_context(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    generation_id: &str,
) -> Result<String, String> {
    let mut lines = Vec::new();
    for variant in list_variants_for_generation(ctx, base_url, headers, generation_id)? {
        let variant_id = entity_id_from_entity(&variant);
        let variant_fields = state_fields(&variant);
        lines.push(format!(
            "Variant {variant_id} status={} app_ref={} runtime_ref={} branch_ref={} summary={}",
            entity_status(&variant),
            compact(&field_str(&variant_fields, &["AppRef"]), 240),
            compact(&field_str(&variant_fields, &["RuntimeRef"]), 240),
            compact(&field_str(&variant_fields, &["BranchRef"]), 240),
            compact(&field_str(&variant_fields, &["Summary"]), 1200),
        ));
        for result in list_stage_results_for_variant(ctx, base_url, headers, &variant_id)? {
            let result_id = entity_id_from_entity(&result);
            let fields = state_fields(&result);
            lines.push(format!(
                "  StageResult {result_id} stage={} status={} summary={} metrics={}",
                field_str(&fields, &["EvaluationStageId"]),
                entity_status(&result),
                compact(&field_str(&fields, &["Summary", "FailureReason"]), 1000),
                compact(&field_str(&fields, &["MetricsJson"]), 1000),
            ));
        }
    }
    if lines.is_empty() {
        lines.push("No variants were recorded for this generation.".to_string());
    }
    Ok(lines.join("\n"))
}

fn trial_evidence_context(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    stage_result_id: &str,
) -> Result<String, String> {
    if stage_result_id.trim().is_empty() {
        return Ok(String::new());
    }
    let filter = format!(
        "StageResultId%20eq%20'{}'",
        escape_odata_id(stage_result_id)
    );
    let mut lines = Vec::new();
    for trial in list_entities(ctx, base_url, headers, "Trials", &filter)? {
        let trial_id = entity_id_from_entity(&trial);
        let fields = state_fields(&trial);
        lines.push(format!(
            "Trial {trial_id} status={} user={} intent_satisfied={} summary={} observations={} friction={}",
            entity_status(&trial),
            compact(&field_str(&fields, &["SimulatedUserId"]), 120),
            compact(&field_str(&fields, &["IntentSatisfied"]), 80),
            compact(&field_str(&fields, &["Summary", "FailureReason"]), 800),
            compact(&field_str(&fields, &["ObservationJson"]), 800),
            compact(&field_str(&fields, &["FrictionJson"]), 600),
        ));
    }
    Ok(lines.join("\n"))
}

fn entity_fields_or_empty(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    entity_set: &str,
    entity_id: &str,
) -> Value {
    if entity_id.trim().is_empty() {
        return json!({});
    }
    get_entity(ctx, base_url, headers, entity_set, entity_id)
        .map(|entity| state_fields(&entity))
        .unwrap_or_else(|_| json!({}))
}

fn compact(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_string();
    }
    let mut out = value
        .chars()
        .take(max_chars.saturating_sub(3))
        .collect::<String>();
    out.push_str("...");
    out
}

fn nonempty(value: String, fallback: String) -> String {
    if value.trim().is_empty() {
        fallback
    } else {
        value
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::*;

    #[test]
    fn evaluation_prompt_includes_runtime_ref() {
        let prompt = format_evaluation_prompt(
            &json!({
                "StageName": "AI simulated user trial",
                "StageKind": "simulated_user_live_trial",
                "RequiredEvidenceJson": "{\"must_use_runtime\":true}"
            }),
            "AdaptationGoal goal-1: make answers useful",
            "",
            "https://genesis-production-164d.up.railway.app",
            "var-1",
            "gen-1",
            "ep-1",
            "direction-1",
            "stage-1",
            "stage-result-1",
            "work-item-1",
            "Variant answers with evidence.",
            "nerdsane/agent-answers@abc123",
            "temper://tenant/de-variant-var-1/app/nerdsane/agent-answers@abc123",
        );

        assert!(prompt.contains("RuntimeRef: temper://tenant/de-variant-var-1/app/nerdsane/agent-answers@abc123"));
        assert!(prompt.contains("x-de-direction-id: direction-1"));
        assert!(prompt.contains("TemperApiBase: https://genesis-production-164d.up.railway.app"));
        assert!(prompt.contains("x-tenant-id set to the tenant from RuntimeRef"));
        assert!(prompt.contains("AppRef: nerdsane/agent-answers@abc123"));
    }

    #[test]
    fn directed_evolution_headers_include_persona_and_run_tags() {
        let headers = directed_evolution_header_block(
            "direction-1",
            "episode-1",
            "generation-1",
            "variant-1",
            "stage-1",
            "stage-result-1",
            "trial-1",
            "work-item-1",
            "sim-user-1",
            "2",
            "3",
            "temper://tenant/de-variant/app/nerdsane/agent-answers@abc",
            "nerdsane/agent-answers@abc",
        );

        assert!(headers.contains("x-de-persona-index: 2"));
        assert!(headers.contains("x-de-run-index: 3"));
    }
}
