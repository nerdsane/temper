fn route_stage_result(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    work_item_id: &str,
    role: &str,
    stage_result_id: &str,
    work_item_fields: &Value,
    output: &Value,
) -> Result<Value, String> {
    let stage_result = get_entity(ctx, base_url, headers, "StageResults", stage_result_id)?;
    let stage_result_fields = state_fields(&stage_result);
    let _episode_id = field_str(&stage_result_fields, &["EpisodeId"]);
    let generation_id = field_str(&stage_result_fields, &["GenerationId"]);
    let variant_id = field_str(&stage_result_fields, &["VariantId"]);
    let mut passed = stage_evaluation_passed(output);
    let missing_datadog_evidence = if passed
        && stage_result_requires_datadog_evidence(ctx, base_url, headers, &stage_result_fields)?
        && !output_has_datadog_evidence_scope(output)
    {
        Some(
            "EvaluationStage required datadog_evidence_scope, but the brain output did not include an evidence_scope item with a Datadog URL."
                .to_string(),
        )
    } else {
        None
    };
    if missing_datadog_evidence.is_some() {
        passed = false;
    }
    let metrics_json = lookup_value_deep(output, &["metrics", "Metrics", "metrics_json"])
        .unwrap_or_else(|| json!({}))
        .to_string();
    let metrics = serde_json::from_str::<Value>(&metrics_json).unwrap_or_else(|_| json!({}));
    let summary = nonempty(
        lookup_string_deep(output, &["summary", "reasoning_summary", "verdict"]),
        field_str(work_item_fields, &["Summary"]),
    );
    let failure_reason = missing_datadog_evidence.unwrap_or_else(|| {
        nonempty(
            lookup_string_deep(output, &["failure_reason", "failureReason", "reason"]),
            summary.clone(),
        )
    });
    let evidence_artifact_id = field_str(work_item_fields, &["EvidenceArtifactId"]);
    let variant = get_entity(ctx, base_url, headers, "Variants", &variant_id)?;
    let variant_status = entity_status(&variant);

    if passed {
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
        maybe_finish_trial_for_stage_result(
            ctx,
            base_url,
            headers,
            role,
            work_item_id,
            true,
            &summary,
            &evidence_artifact_id,
            &metrics_json,
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
                "EliminationRuleId": "",
                "StageResultId": stage_result_id,
                "EvidenceArtifactId": evidence_artifact_id,
                "Reason": failure_reason,
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
    maybe_finish_trial_for_stage_result(
        ctx,
        base_url,
        headers,
        role,
        work_item_id,
        false,
        &failure_reason,
        &evidence_artifact_id,
        &metrics_json,
    )?;
    let selection_work_item_id =
        maybe_finish_generation_after_evaluation(ctx, base_url, headers, &generation_id)?;

    Ok(json!({
        "routed": "stage_result",
        "stage_result_id": stage_result_id,
        "variant_id": variant_id,
        "passed": false,
        "measurement_ids": measurement_ids,
        "selection_work_item_id": selection_work_item_id,
    }))
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

fn stage_result_requires_datadog_evidence(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    stage_result_fields: &Value,
) -> Result<bool, String> {
    let stage_id = field_str(stage_result_fields, &["EvaluationStageId"]);
    if stage_id.trim().is_empty() {
        return Ok(false);
    }
    let stage = get_entity(ctx, base_url, headers, "EvaluationStages", &stage_id)?;
    let stage_fields = state_fields(&stage);
    Ok(required_evidence_includes_datadog(&field_str(
        &stage_fields,
        &["RequiredEvidenceJson"],
    )))
}

fn required_evidence_includes_datadog(raw: &str) -> bool {
    if raw
        .to_ascii_lowercase()
        .contains("datadog_evidence_scope")
    {
        return true;
    }
    serde_json::from_str::<Value>(raw)
        .map(|value| value_contains_datadog_requirement(&value))
        .unwrap_or(false)
}

fn value_contains_datadog_requirement(value: &Value) -> bool {
    match value {
        Value::String(value) => value.eq_ignore_ascii_case("datadog_evidence_scope"),
        Value::Array(items) => items.iter().any(value_contains_datadog_requirement),
        Value::Object(object) => object.values().any(value_contains_datadog_requirement),
        _ => false,
    }
}

fn output_has_datadog_evidence_scope(output: &Value) -> bool {
    for key in ["evidence_scope", "evidenceScope"] {
        let Some(value) = lookup_value_deep(output, &[key]) else {
            continue;
        };
        let Some(items) = value.as_array() else {
            continue;
        };
        for item in items {
            let url = lookup_string_deep(item, &["datadog_url", "datadogUrl"]);
            let Some(url) = url
                .split_whitespace()
                .next()
                .filter(|value| !value.is_empty())
            else {
                continue;
            };
            if is_datadog_app_url(url) {
                return true;
            }
        }
    }
    false
}

fn is_datadog_app_url(url: &str) -> bool {
    [
        "https://app.datadoghq.com",
        "https://app.us3.datadoghq.com",
        "https://app.us5.datadoghq.com",
        "https://app.datadoghq.eu",
        "https://app.ap1.datadoghq.com",
        "https://app.ap2.datadoghq.com",
        "https://app.ddog-gov.com",
    ]
    .iter()
    .any(|prefix| url.starts_with(prefix))
}
