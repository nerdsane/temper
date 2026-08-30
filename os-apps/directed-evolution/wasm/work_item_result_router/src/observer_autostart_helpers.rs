fn repair_autostart_lane_allowed(pressure_class: &str, autonomy_lane: &str) -> bool {
    let text = format!("{pressure_class} {autonomy_lane}").to_ascii_lowercase();
    text.contains("repair")
        && (text.contains("auto") || text.contains("automatic"))
        && !contains_any(
            &text,
            &[
                "growth",
                "feature",
                "product",
                "ux",
                "policy",
                "data-model",
                "data model",
            ],
        )
}

fn active_autonomy_policy_for_organism(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    organism_id: &str,
) -> Result<Option<Value>, String> {
    let value = get_json(
        ctx,
        &format!("{base_url}/tdata/AutonomyPolicies?$top=100"),
        headers,
    )?;
    let policies = value
        .get("value")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for policy in policies {
        let fields = state_fields(&policy);
        if entity_status(&policy) == "Active" && field_str(&fields, &["OrganismId"]) == organism_id
        {
            return Ok(Some(policy));
        }
    }
    Ok(None)
}

fn policy_permits_repair_autostart(policy_json: &str) -> bool {
    let parsed = serde_json::from_str::<Value>(policy_json).unwrap_or_else(|_| json!({}));
    let repair_text = parsed
        .get("repair_lane")
        .or_else(|| parsed.get("repairLane"))
        .map(|value| format!("repair_lane {}", value))
        .unwrap_or_else(|| policy_json.to_string())
        .to_ascii_lowercase();
    !contains_any(
        &repair_text,
        &["human approval", "human-gated", "human gated", "blocked", "never"],
    ) && (repair_text.contains("auto") || repair_text.contains("automatic"))
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

fn activate_repair_metric_definitions(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    output: &Value,
    brain_run_id: &str,
) -> Result<Vec<String>, String> {
    let mut metrics = metric_definitions_from_output(output);
    if metrics.is_empty() {
        metrics = vec![
            RepairMetricDefinition {
                name: "baseline_regression_count".to_string(),
                kind: "regression".to_string(),
                unit: "count".to_string(),
                higher_is_better: "false".to_string(),
                description: "Number of baseline Agent Answers behaviors regressed by the repair.".to_string(),
            },
            RepairMetricDefinition {
                name: "simulated_user_repair_success".to_string(),
                kind: "repair_outcome".to_string(),
                unit: "boolean".to_string(),
                higher_is_better: "true".to_string(),
                description: "Whether AI simulated users observe the repair resolving the failure.".to_string(),
            },
        ];
    }

    let mut ids = Vec::new();
    for metric in metrics {
        let metric_id = create_entity(ctx, base_url, headers, "MetricDefinitions")?;
        post_directed_action(
            ctx,
            base_url,
            headers,
            "MetricDefinitions",
            &metric_id,
            "ActivateMetricDefinition",
            json!({
                "MetricName": metric.name,
                "MetricKind": metric.kind,
                "Unit": metric.unit,
                "HigherIsBetter": metric.higher_is_better,
                "Description": format!("{} Created by {brain_run_id}.", metric.description),
            }),
        )?;
        ids.push(metric_id);
    }
    Ok(ids)
}

fn activate_repair_constraints(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode_id: &str,
    proposed_constraints: &str,
    brain_run_id: &str,
) -> Result<Vec<String>, String> {
    let mut constraints = parse_json_string_array(proposed_constraints);
    if constraints.is_empty() {
        constraints = vec![
            "Preserve existing Question and Answer actions and fields.".to_string(),
            "Do not modify evaluators, selection pressure, or viability constraints from inside a variant.".to_string(),
        ];
    }

    let mut ids = Vec::new();
    for (index, constraint) in constraints.into_iter().enumerate() {
        let constraint_id = create_entity(ctx, base_url, headers, "ViabilityConstraints")?;
        post_directed_action(
            ctx,
            base_url,
            headers,
            "ViabilityConstraints",
            &constraint_id,
            "ActivateViabilityConstraint",
            json!({
                "EpisodeId": episode_id,
                "ConstraintStatement": constraint,
                "ConstraintKind": if index == 0 { "repair-boundary" } else { "regression" },
                "CreatedByBrainRunId": brain_run_id,
            }),
        )?;
        ids.push(constraint_id);
    }
    Ok(ids)
}

fn activate_repair_evaluation_stages(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode_id: &str,
    output: &Value,
) -> Result<Vec<String>, String> {
    let mut stages = evaluation_stages_from_output(output);
    if stages.is_empty() {
        stages = default_repair_evaluation_stages();
    }
    ensure_required_repair_stages(&mut stages);

    let mut ids = Vec::new();
    for (index, stage) in stages.into_iter().enumerate() {
        let stage_id = create_entity(ctx, base_url, headers, "EvaluationStages")?;
        post_directed_action(
            ctx,
            base_url,
            headers,
            "EvaluationStages",
            &stage_id,
            "ActivateEvaluationStage",
            json!({
                "EpisodeId": episode_id,
                "StageName": stage.name,
                "StageKind": stage.kind,
                "SequenceIndex": (index + 1).to_string(),
                "RequiredEvidenceJson": json!(stage.required_evidence).to_string(),
                "ExecutorKind": stage.executor,
            }),
        )?;
        ids.push(stage_id);
    }
    Ok(ids)
}

#[derive(Clone)]
struct RepairMetricDefinition {
    name: String,
    kind: String,
    unit: String,
    higher_is_better: String,
    description: String,
}

fn metric_definitions_from_output(output: &Value) -> Vec<RepairMetricDefinition> {
    let Some(value) = lookup_value_deep(output, &["metric_definitions", "metrics"]) else {
        return Vec::new();
    };
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = lookup_string_deep(item, &["metric_name", "name", "MetricName"]);
                    (!name.trim().is_empty()).then(|| RepairMetricDefinition {
                        name,
                        kind: nonempty(
                            lookup_string_deep(item, &["metric_kind", "kind", "MetricKind"]),
                            "repair_outcome".to_string(),
                        ),
                        unit: nonempty(
                            lookup_string_deep(item, &["unit", "Unit"]),
                            "score".to_string(),
                        ),
                        higher_is_better: nonempty(
                            lookup_string_deep(
                                item,
                                &["higher_is_better", "HigherIsBetter"],
                            ),
                            "true".to_string(),
                        ),
                        description: nonempty(
                            lookup_string_deep(item, &["description", "Description"]),
                            "Observer-brain proposed repair metric.".to_string(),
                        ),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Clone)]
struct RepairEvaluationStage {
    name: String,
    kind: String,
    executor: String,
    required_evidence: Vec<String>,
}

fn evaluation_stages_from_output(output: &Value) -> Vec<RepairEvaluationStage> {
    let Some(value) = lookup_value_deep(output, &["evaluation_stages", "EvaluationStages"]) else {
        return Vec::new();
    };
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let name = lookup_string_deep(item, &["stage_name", "name", "StageName"]);
                    (!name.trim().is_empty()).then(|| RepairEvaluationStage {
                        name,
                        kind: nonempty(
                            lookup_string_deep(item, &["stage_kind", "kind", "StageKind"]),
                            "reviewer".to_string(),
                        ),
                        executor: nonempty(
                            lookup_string_deep(item, &["executor_kind", "executor", "ExecutorKind"]),
                            "codex".to_string(),
                        ),
                        required_evidence: string_array_from_value(lookup_value_deep(
                            item,
                            &["required_evidence", "RequiredEvidence"],
                        )),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn default_repair_evaluation_stages() -> Vec<RepairEvaluationStage> {
    vec![
        RepairEvaluationStage {
            name: "Code and spec review".to_string(),
            kind: "reviewer".to_string(),
            executor: "codex".to_string(),
            required_evidence: vec![
                "changed_files".to_string(),
                "verification_notes".to_string(),
            ],
        },
        RepairEvaluationStage {
            name: "AI simulated user repair trial".to_string(),
            kind: "simulated_user".to_string(),
            executor: "codex".to_string(),
            required_evidence: vec![
                "simulated_user_trace".to_string(),
                "datadog_evidence_scope".to_string(),
            ],
        },
    ]
}

fn ensure_required_repair_stages(stages: &mut Vec<RepairEvaluationStage>) {
    let has_review = stages
        .iter()
        .any(|stage| !stage.kind.to_ascii_lowercase().contains("simulated"));
    let has_simulated_user = stages
        .iter()
        .any(|stage| stage.kind.to_ascii_lowercase().contains("simulated"));
    if !has_review {
        stages.push(default_repair_evaluation_stages()[0].clone());
    }
    if !has_simulated_user {
        stages.push(default_repair_evaluation_stages()[1].clone());
    }
}

fn string_array_from_value(value: Option<Value>) -> Vec<String> {
    value
        .and_then(|value| {
            value.as_array().map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
        })
        .unwrap_or_default()
}
