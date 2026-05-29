fn route_stage_result(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    _work_item_id: &str,
    role: &str,
    stage_result_id: &str,
    work_item_fields: &Value,
    output: &Value,
) -> Result<Value, String> {
    let stage_result = get_entity(ctx, base_url, headers, "StageResults", stage_result_id)?;
    let stage_result_fields = state_fields(&stage_result);
    let episode_id = field_str(&stage_result_fields, &["EpisodeId"]);
    let generation_id = field_str(&stage_result_fields, &["GenerationId"]);
    let variant_id = field_str(&stage_result_fields, &["VariantId"]);
    let stage_id = field_str(&stage_result_fields, &["EvaluationStageId"]);
    let stage = get_entity(ctx, base_url, headers, "EvaluationStages", &stage_id)?;
    let stage_fields = state_fields(&stage);
    let episode = get_entity(ctx, base_url, headers, "Episodes", &episode_id)?;
    let episode_fields = state_fields(&episode);
    let raw_metrics_json = lookup_value_deep(output, &["metrics", "Metrics", "metrics_json"])
        .unwrap_or_else(|| json!({}))
        .to_string();
    let mut metrics = serde_json::from_str::<Value>(&raw_metrics_json).unwrap_or_else(|_| json!({}));
    let trial_state_counts = inject_simulated_user_trial_state_metrics(
        ctx,
        base_url,
        headers,
        role,
        &stage_fields,
        stage_result_id,
        &mut metrics,
    )?;
    let metrics_json = metrics.to_string();
    let summary = nonempty(
        lookup_string_deep(output, &["summary", "reasoning_summary", "verdict"]),
        field_str(work_item_fields, &["Summary"]),
    );
    let failure_reason = nonempty(
        lookup_string_deep(output, &["failure_reason", "failureReason", "reason"]),
        summary.clone(),
    );
    let evidence_artifact_id = field_str(work_item_fields, &["EvidenceArtifactId"]);
    let variant = get_entity(ctx, base_url, headers, "Variants", &variant_id)?;
    let variant_status = entity_status(&variant);
    let decision = enforced_stage_decision(
        ctx,
        base_url,
        headers,
        &episode_fields,
        &stage_fields,
        role,
        output,
        &metrics,
        stage_result_id,
        &summary,
        &failure_reason,
    )?;

    if decision.passed {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "StageResults",
            stage_result_id,
            "PassStageResult",
            json!({
                "MetricsJson": metrics_json,
                "EvidenceArtifactId": evidence_artifact_id,
                "Summary": summary,
                "EvaluatorRole": role,
                "ProvenanceKind": decision.provenance_kind,
                "DecisionBasisJson": decision.decision_basis_json,
                "InputsJson": decision.inputs_json,
            }),
        )?;
        if variant_status == "Active" {
            post_directed_action(
                ctx,
                base_url,
                headers,
                "Variants",
                &variant_id,
                "RecordVariantStageResult",
                json!({ "StageResultId": stage_result_id }),
            )?;
        }
        let measurement_ids = record_measurements(
            ctx,
            base_url,
            headers,
            &metrics,
            stage_result_id,
            &variant_id,
            &evidence_artifact_id,
        )?;
        if variant_status == "Active" {
            maybe_record_generation_survivor(ctx, base_url, headers, &generation_id, &variant_id)?;
        }
        let selection_work_item_id =
            maybe_finish_generation_after_evaluation(ctx, base_url, headers, &generation_id)?;
        return Ok(json!({
            "routed": "stage_result",
            "stage_result_id": stage_result_id,
            "variant_id": variant_id,
            "passed": true,
            "measurement_ids": measurement_ids,
            "trial_state_counts": trial_state_counts.as_ref().map(TrialStateCounts::to_json),
            "selection_work_item_id": selection_work_item_id,
        }));
    }

    post_directed_action(
        ctx,
        base_url,
        headers,
        "StageResults",
        stage_result_id,
        "FailStageResult",
        json!({
                "MetricsJson": metrics_json,
                "EvidenceArtifactId": evidence_artifact_id,
                "FailureReason": decision.failure_reason,
                "EvaluatorRole": role,
                "ProvenanceKind": decision.provenance_kind,
                "DecisionBasisJson": decision.decision_basis_json,
                "InputsJson": decision.inputs_json,
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
            "EliminationRuleId": decision.elimination_rule_id,
            "EvidenceArtifactId": evidence_artifact_id,
            "Reason": decision.failure_reason,
        }),
    )?;
    if matches!(
        variant_status.as_str(),
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
                "EliminationRuleId": decision.elimination_rule_id,
                "StageResultId": stage_result_id,
                "EvidenceArtifactId": evidence_artifact_id,
                "Reason": decision.failure_reason,
            }),
        )?;
    }
    let measurement_ids = record_measurements(
        ctx,
        base_url,
        headers,
        &metrics,
        stage_result_id,
        &variant_id,
        &evidence_artifact_id,
    )?;
    let selection_work_item_id =
        maybe_finish_generation_after_evaluation(ctx, base_url, headers, &generation_id)?;

    Ok(json!({
        "routed": "stage_result",
        "stage_result_id": stage_result_id,
        "variant_id": variant_id,
        "passed": false,
        "measurement_ids": measurement_ids,
        "trial_state_counts": trial_state_counts.as_ref().map(TrialStateCounts::to_json),
        "selection_work_item_id": selection_work_item_id,
    }))
}

struct EnforcedStageDecision {
    passed: bool,
    failure_reason: String,
    elimination_rule_id: String,
    provenance_kind: String,
    decision_basis_json: String,
    inputs_json: String,
}

fn enforced_stage_decision(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode_fields: &Value,
    stage_fields: &Value,
    role: &str,
    output: &Value,
    metrics: &Value,
    stage_result_id: &str,
    summary: &str,
    fallback_failure_reason: &str,
) -> Result<EnforcedStageDecision, String> {
    let mut passed = stage_evaluation_passed(output);
    let mut provenance_kind = nonempty(
        lookup_string_deep(
            output,
            &[
                "provenance_kind",
                "provenanceKind",
                "EvidenceProvenance",
                "evidence_provenance",
            ],
        ),
        default_provenance_for_role(role).to_string(),
    );
    let decision_basis_json = lookup_value_deep(output, &["decision_basis", "decisionBasis"])
        .unwrap_or_else(|| json!({ "summary": summary }))
        .to_string();
    let inputs_json = lookup_value_deep(output, &["inputs", "Inputs"])
        .unwrap_or_else(|| json!({}))
        .to_string();
    let mut failure_reason = nonempty(
        lookup_string_deep(output, &["failure_reason", "failureReason", "reason"]),
        fallback_failure_reason.to_string(),
    );
    let mut elimination_rule_id = String::new();

    if stage_requires_datadog(stage_fields)
        && !datadog_evidence_satisfies_required_contract(output, &provenance_kind)
    {
        passed = false;
        failure_reason = "Datadog-measured stage failed closed because the evaluator output did not include structured Datadog evidence with query, time window, result count, interpretation, and explicit zero-result meaning.".to_string();
    }

    if let Some((rule_id, reason, threshold_provenance_kind)) = violated_threshold_rule(
        ctx,
        base_url,
        headers,
        episode_fields,
        metrics,
        stage_result_id,
    )? {
        passed = false;
        elimination_rule_id = rule_id;
        failure_reason = reason;
        provenance_kind = threshold_provenance_kind;
    }

    Ok(EnforcedStageDecision {
        passed,
        failure_reason,
        elimination_rule_id,
        provenance_kind,
        decision_basis_json,
        inputs_json,
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct TrialStateCounts {
    total: usize,
    succeeded: usize,
    failed: usize,
    blocked: usize,
    app_blocked: usize,
    runtime_blocked: usize,
    ambiguous_blocked: usize,
    nonterminal: usize,
}

impl TrialStateCounts {
    fn to_json(&self) -> Value {
        json!({
            "total": self.total,
            "succeeded": self.succeeded,
            "failed": self.failed,
            "blocked": self.blocked,
            "app_blocked": self.app_blocked,
            "runtime_blocked": self.runtime_blocked,
            "ambiguous_blocked": self.ambiguous_blocked,
            "nonterminal": self.nonterminal,
        })
    }
}

fn inject_simulated_user_trial_state_metrics(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    role: &str,
    stage_fields: &Value,
    stage_result_id: &str,
    metrics: &mut Value,
) -> Result<Option<TrialStateCounts>, String> {
    if !stage_should_use_trial_state_metrics(role, stage_fields) {
        return Ok(None);
    }

    let trial_filter = format!(
        "StageResultId%20eq%20'{}'",
        escape_odata_id(stage_result_id)
    );
    let trials = list_entities(ctx, base_url, headers, "Trials", &trial_filter)?;
    let counts = trial_state_counts_from_entities(&trials);
    upsert_metric(
        metrics,
        "simulated_user_trial_count",
        counts.total as f64,
        "trials",
        "state-verified",
        "Total recorded simulated-user Trial entities for this StageResult.",
    );
    upsert_metric(
        metrics,
        "simulated_user_success_count",
        counts.succeeded as f64,
        "trials",
        "state-verified",
        "Recorded simulated-user Trial entities that reached Succeeded.",
    );
    upsert_metric(
        metrics,
        "simulated_user_failed_trial_count",
        counts.failed as f64,
        "trials",
        "state-verified",
        "Recorded simulated-user Trial entities that reached Failed.",
    );
    upsert_metric(
        metrics,
        "simulated_user_blocker_count",
        counts.blocked as f64,
        "trials",
        "state-verified",
        "Recorded simulated-user Trial entities that failed or carried a blocker.",
    );
    upsert_metric(
        metrics,
        "simulated_user_app_blocker_count",
        counts.app_blocked as f64,
        "trials",
        "state-verified",
        "Recorded simulated-user Trial entities whose blocker was classified as app behavior.",
    );
    upsert_metric(
        metrics,
        "simulated_user_runtime_blocker_count",
        counts.runtime_blocked as f64,
        "trials",
        "state-verified",
        "Recorded simulated-user Trial entities whose blocker was classified as runtime access or routing.",
    );
    upsert_metric(
        metrics,
        "simulated_user_ambiguous_blocker_count",
        counts.ambiguous_blocked as f64,
        "trials",
        "state-verified",
        "Recorded simulated-user Trial entities whose blocker could not be deterministically classified.",
    );
    upsert_metric(
        metrics,
        "simulated_user_nonterminal_count",
        counts.nonterminal as f64,
        "trials",
        "state-verified",
        "Recorded simulated-user Trial entities that were not terminal when this evaluator result was routed.",
    );
    Ok(Some(counts))
}

fn stage_should_use_trial_state_metrics(role: &str, stage_fields: &Value) -> bool {
    let kind = field_str(stage_fields, &["StageKind"]).to_ascii_lowercase();
    role == "viability_evaluator" || kind.contains("simulated")
}

fn trial_state_counts_from_entities(trials: &[Value]) -> TrialStateCounts {
    let mut counts = TrialStateCounts::default();
    for trial in trials {
        counts.total += 1;
        let fields = state_fields(trial);
        let status = nonempty(
            entity_status(trial),
            field_str(&fields, &["Status", "status"]),
        );
        let status = status.to_ascii_lowercase();
        let blocker = field_str(&fields, &["Blocker", "blocker"]);
        let failure_reason = field_str(&fields, &["FailureReason", "failure_reason"]);
        let blocked = !blocker.trim().is_empty()
            || (status == "failed" && !failure_reason.trim().is_empty())
            || status == "failed";
        match status.as_str() {
            "succeeded" => counts.succeeded += 1,
            "failed" => counts.failed += 1,
            "archived" => {}
            _ => counts.nonterminal += 1,
        }
        if blocked {
            counts.blocked += 1;
            match trial_blocker_kind(&fields, &blocker, &failure_reason).as_str() {
                "app-behavior" | "variant-behavior" => counts.app_blocked += 1,
                "runtime-access" | "environment" | "runtime" => counts.runtime_blocked += 1,
                _ => counts.ambiguous_blocked += 1,
            }
        }
    }
    counts
}

fn trial_blocker_kind(fields: &Value, blocker: &str, failure_reason: &str) -> String {
    let raw = field_str(
        fields,
        &[
            "BlockerKind",
            "blocker_kind",
            "blockerKind",
            "BlockerScope",
            "blocker_scope",
        ],
    )
    .to_ascii_lowercase();
    let normalized = raw.trim().replace('_', "-");
    if matches!(
        normalized.as_str(),
        "app-behavior"
            | "variant-behavior"
            | "runtime-access"
            | "environment"
            | "runtime"
            | "ambiguous"
    ) {
        return normalized;
    }
    classify_trial_blocker_kind(blocker, failure_reason)
}

fn classify_trial_blocker_kind(blocker: &str, failure_reason: &str) -> String {
    let haystack = format!("{blocker}\n{failure_reason}").to_ascii_lowercase();
    if haystack.contains("no live app route")
        || haystack.contains("no route")
        || haystack.contains("router-level")
        || haystack.contains("404")
        || haystack.contains("metadata")
        || haystack.contains("/tdata")
        || haystack.contains("runtime host")
        || haystack.contains("gateway")
        || haystack.contains("unreachable")
        || haystack.contains("unavailable")
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

fn upsert_metric(
    metrics: &mut Value,
    name: &str,
    value: f64,
    unit: &str,
    provenance_kind: &str,
    interpretation: &str,
) {
    let metric = json!({
        "value": value,
        "unit": unit,
        "provenance_kind": provenance_kind,
        "interpretation": interpretation,
    });
    if let Some(object) = metrics.as_object_mut() {
        object.insert(name.to_string(), metric);
        return;
    }
    if let Some(items) = metrics.as_array_mut() {
        items.push(json!({
            "metric_definition_id": name,
            "value": value,
            "unit": unit,
            "provenance_kind": provenance_kind,
            "interpretation": interpretation,
        }));
        return;
    }
    let mut object = serde_json::Map::new();
    object.insert(name.to_string(), metric);
    *metrics = Value::Object(object);
}

fn stage_requires_datadog(stage_fields: &Value) -> bool {
    let provenance = field_str(stage_fields, &["MeasurementProvenance"]).to_ascii_lowercase();
    let required = field_str(stage_fields, &["RequiredEvidenceJson"]).to_ascii_lowercase();
    let kind = field_str(stage_fields, &["StageKind"]).to_ascii_lowercase();
    provenance.contains("datadog")
        || required.contains("datadog")
        || kind.contains("datadog")
        || kind.contains("telemetry")
}

fn datadog_evidence_satisfies_required_contract(output: &Value, provenance_kind: &str) -> bool {
    if provenance_kind != "datadog-measured" {
        return false;
    }
    let Some(items) = output
        .get("evidence_scope")
        .or_else(|| output.get("evidenceScope"))
        .and_then(Value::as_array)
    else {
        return false;
    };
    items.iter().any(|item| {
        let query = field_str(item, &["query", "Query"]);
        let time_window = field_str(item, &["time_window", "timeWindow", "TimeWindow"]);
        let result_count = field_value_string(item, &["result_count", "resultCount", "ResultCount", "count"]);
        let interpretation = field_str(item, &["interpretation", "Interpretation", "result_summary", "resultSummary"]);
        let zero = field_str(item, &["zero_result_meaning", "zeroResultMeaning", "ZeroResultMeaning"]).to_ascii_lowercase();
        if query.trim().is_empty()
            || time_window.trim().is_empty()
            || result_count.trim().is_empty()
            || interpretation.trim().is_empty()
            || zero.trim().is_empty()
        {
            return false;
        }
        let count = result_count.parse::<i64>().unwrap_or(1);
        count > 0 || zero == "success"
    })
}

fn field_value_string(fields: &Value, keys: &[&str]) -> String {
    for key in keys {
        if let Some(value) = fields.get(*key) {
            if let Some(text) = value.as_str() {
                return text.to_string();
            }
            if let Some(number) = value.as_i64() {
                return number.to_string();
            }
            if let Some(number) = value.as_u64() {
                return number.to_string();
            }
            if let Some(number) = value.as_f64() {
                return number.to_string();
            }
        }
        let lower = lower_first(key);
        if let Some(value) = fields.get(&lower) {
            if let Some(text) = value.as_str() {
                return text.to_string();
            }
            if let Some(number) = value.as_i64() {
                return number.to_string();
            }
            if let Some(number) = value.as_u64() {
                return number.to_string();
            }
            if let Some(number) = value.as_f64() {
                return number.to_string();
            }
        }
    }
    String::new()
}

fn violated_threshold_rule(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode_fields: &Value,
    metrics: &Value,
    stage_result_id: &str,
) -> Result<Option<(String, String, String)>, String> {
    for rule_id in parse_json_string_array(&field_str(episode_fields, &["EliminationRuleIdsJson"])) {
        let rule = get_entity(ctx, base_url, headers, "EliminationRules", &rule_id)?;
        let rule_fields = state_fields(&rule);
        let threshold = field_str(&rule_fields, &["ThresholdJson"]);
        let Some(object) = serde_json::from_str::<Value>(&threshold)
            .ok()
            .and_then(|value| value.as_object().cloned())
        else {
            continue;
        };
        for (metric_name, limit_value) in object {
            let Some(actual) = metric_numeric_value(metrics, &metric_name) else {
                continue;
            };
            let Some(limit) = limit_value.as_f64().or_else(|| {
                limit_value
                    .as_str()
                    .and_then(|raw| raw.parse::<f64>().ok())
            }) else {
                continue;
            };
            if actual > limit {
                let provenance_kind = metric_provenance_kind(metrics, &metric_name)
                    .unwrap_or_else(|| "wasm-computed".to_string());
                return Ok(Some((
                    rule_id,
                    format!(
                        "StageResult {stage_result_id} violated elimination rule: metric {metric_name}={actual} exceeded threshold {limit}."
                    ),
                    provenance_kind,
                )));
            }
        }
    }
    Ok(None)
}

fn metric_provenance_kind(metrics: &Value, metric_name: &str) -> Option<String> {
    if let Some(object) = metrics.as_object() {
        let value = object.get(metric_name)?;
        return value
            .get("provenance_kind")
            .or_else(|| value.get("provenanceKind"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }
    if let Some(items) = metrics.as_array() {
        for item in items {
            let name = lookup_string_deep(item, &["metric_definition_id", "MetricDefinitionId", "metric", "name"]);
            if name == metric_name {
                return item
                    .get("provenance_kind")
                    .or_else(|| item.get("provenanceKind"))
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
        }
    }
    None
}

fn metric_numeric_value(metrics: &Value, metric_name: &str) -> Option<f64> {
    if let Some(object) = metrics.as_object() {
        let value = object.get(metric_name)?;
        return value.as_f64().or_else(|| {
            value
                .as_str()
                .and_then(|raw| raw.trim_matches('"').parse::<f64>().ok())
                .or_else(|| {
                    value
                        .get("value")
                        .or_else(|| value.get("Value"))
                        .and_then(|inner| {
                            inner
                                .as_f64()
                                .or_else(|| inner.as_str().and_then(|raw| raw.parse::<f64>().ok()))
                        })
                })
        });
    }
    if let Some(items) = metrics.as_array() {
        for item in items {
            let name = lookup_string_deep(item, &["metric_definition_id", "MetricDefinitionId", "metric", "name"]);
            if name == metric_name {
                return lookup_string_deep(item, &["value", "Value"]).parse::<f64>().ok();
            }
        }
    }
    None
}

fn default_provenance_for_role(role: &str) -> &'static str {
    match role {
        "state_verifier" => "state-verified",
        "telemetry_evaluator" => "datadog-measured",
        "wasm_evaluator" => "wasm-computed",
        "viability_evaluator" | "reviewer" => "brain-judged",
        _ => "agent-observed",
    }
}

fn stage_evaluation_passed(output: &Value) -> bool {
    if let Some(passed) = lookup_bool_deep(output, &["passed", "success", "viable"]) {
        return passed;
    }

    let status = lookup_string_deep(output, &["status", "verdict"]).to_ascii_lowercase();
    if status.trim().is_empty() {
        return false;
    }

    status.contains("pass")
        || status.contains("viable")
        || status.contains("approved")
        || status.contains("acceptable")
}
