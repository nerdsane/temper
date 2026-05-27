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
    variant_summary: &str,
    app_ref: &str,
) -> Result<String, String> {
    let stage_fields = state_fields(stage);
    let contract_context = episode_contract_context(ctx, base_url, headers, episode_fields)?;
    let variant_fields = entity_fields_or_empty(ctx, base_url, headers, "Variants", variant_id);
    let runtime_ref = field_str(&variant_fields, &["RuntimeRef"]);
    let prompt_api_base = resolve_public_api_url(ctx);
    Ok(format_evaluation_prompt(
        &stage_fields,
        &contract_context,
        &prompt_api_base,
        variant_id,
        generation_id,
        episode_id,
        stage_id,
        variant_summary,
        app_ref,
        &runtime_ref,
    ))
}

fn format_evaluation_prompt(
    stage_fields: &Value,
    contract_context: &str,
    temper_api_base: &str,
    variant_id: &str,
    generation_id: &str,
    episode_id: &str,
    stage_id: &str,
    variant_summary: &str,
    app_ref: &str,
    runtime_ref: &str,
) -> String {
    format!(
        "Evaluate Directed Evolution variant.\n\
EpisodeId: {episode_id}\n\
GenerationId: {generation_id}\n\
VariantId: {variant_id}\n\
EvaluationStageId: {stage_id}\n\
StageName: {}\n\
StageKind: {}\n\
RequiredEvidence: {}\n\
TemperApiBase: {temper_api_base}\n\
AppRef: {app_ref}\n\
RuntimeRef: {runtime_ref}\n\
VariantSummary: {variant_summary}\n\n\
EpisodeContract:\n{contract_context}\n\n\
Use the stage contract, Adaptation Goal, Viability Constraints, Selection Pressure, and real evidence. \
If RuntimeRef is a temper://tenant/<tenant>/app/<app_ref> value, exercise that live tenant through \
TemperApiBase /tdata OData calls with x-tenant-id set to the tenant from RuntimeRef. \
If RequiredEvidence includes datadog_evidence_scope, Datadog evidence is mandatory: use authenticated Datadog tools to inspect logs, traces, or metrics tied to the variant tenant/app, include a Datadog URL in evidence_scope, and return passed=false with a clear failure_reason if Datadog cannot be queried. \
Otherwise inspect Datadog logs, traces, or metrics for errors and latency tied to the variant tenant when available. \
Return JSON with: passed, status, summary, metrics, evidence_scope, evidence_refs, failure_reason, and next_actions. \
Each evidence_scope entry should include surface, query, result_summary, and datadog_url when it came from Datadog. \
Do not modify evaluators or selection rules.",
        field_str(stage_fields, &["StageName"]),
        field_str(stage_fields, &["StageKind"]),
        compact(&field_str(stage_fields, &["RequiredEvidenceJson"]), 1000),
    )
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
            "https://genesis-production-164d.up.railway.app",
            "var-1",
            "gen-1",
            "ep-1",
            "stage-1",
            "Variant answers with evidence.",
            "nerdsane/agent-answers@abc123",
            "temper://tenant/de-variant-var-1/app/nerdsane/agent-answers@abc123",
        );

        assert!(prompt.contains("RuntimeRef: temper://tenant/de-variant-var-1/app/nerdsane/agent-answers@abc123"));
        assert!(prompt.contains("TemperApiBase: https://genesis-production-164d.up.railway.app"));
        assert!(prompt.contains("x-tenant-id set to the tenant from RuntimeRef"));
        assert!(prompt.contains("AppRef: nerdsane/agent-answers@abc123"));
        assert!(prompt.contains("evidence_scope"));
        assert!(prompt.contains("datadog_url"));
    }
}
