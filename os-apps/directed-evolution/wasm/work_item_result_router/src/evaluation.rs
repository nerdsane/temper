fn record_measurements(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    metrics: &Value,
    stage_result_id: &str,
    variant_id: &str,
    evidence_artifact_id: &str,
) -> Result<Vec<String>, String> {
    let mut records = Vec::new();
    if let Some(items) = metrics.as_array() {
        for item in items {
            let metric_name = nonempty(
                lookup_string_deep(
                    item,
                    &[
                        "metric_definition_id",
                        "MetricDefinitionId",
                        "metric",
                        "name",
                    ],
                ),
                "metric".to_string(),
            );
            let value = nonempty(
                lookup_string_deep(item, &["value", "Value"]),
                item.to_string(),
            );
            let unit = lookup_string_deep(item, &["unit", "Unit"]);
            let provenance = lookup_string_deep(
                item,
                &["provenance_kind", "provenanceKind", "ProvenanceKind"],
            );
            let interpretation = lookup_string_deep(item, &["interpretation", "Interpretation"]);
            records.push((metric_name, value, unit, provenance, interpretation));
        }
    } else if let Some(object) = metrics.as_object() {
        for (metric_name, value) in object {
            if value.is_object() {
                records.push((
                    metric_name.clone(),
                    nonempty(
                        lookup_string_deep(value, &["value", "Value"]),
                        value.to_string(),
                    ),
                    lookup_string_deep(value, &["unit", "Unit"]),
                    lookup_string_deep(value, &["provenance_kind", "provenanceKind", "ProvenanceKind"]),
                    lookup_string_deep(value, &["interpretation", "Interpretation"]),
                ));
            } else {
                records.push((
                    metric_name.clone(),
                    value.to_string(),
                    String::new(),
                    String::new(),
                    String::new(),
                ));
            }
        }
    }

    let mut measurement_ids = Vec::new();
    for (metric_definition_id, value, unit, provenance_kind, interpretation) in records {
        let measurement_id = create_entity(ctx, base_url, headers, "Measurements")?;
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Measurements",
            &measurement_id,
            "RecordMeasurement",
            json!({
                "MetricDefinitionId": metric_definition_id,
                "StageResultId": stage_result_id,
                "TrialId": "",
                "VariantId": variant_id,
                "Value": value,
                "Unit": unit,
                "EvidenceArtifactId": evidence_artifact_id,
                "ProvenanceKind": provenance_kind,
                "MeasurementKind": "",
                "SourceRunId": "",
                "ComputedByRef": "",
                "Interpretation": interpretation,
            }),
        )?;
        measurement_ids.push(measurement_id);
    }
    Ok(measurement_ids)
}

fn maybe_finish_trial_for_stage_result(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    role: &str,
    work_item_id: &str,
    passed: bool,
    summary: &str,
    evidence_artifact_id: &str,
    measurements_json: &str,
) -> Result<(), String> {
    if role != "simulated_user" {
        return Ok(());
    }
    let filter = format!("WorkItemId%20eq%20'{}'", escape_odata_id(work_item_id));
    for trial in list_entities(ctx, base_url, headers, "Trials", &filter)? {
        if entity_status(&trial) != "Running" {
            continue;
        }
        let trial_id = entity_id_from_entity(&trial);
        let action = if passed { "SucceedTrial" } else { "FailTrial" };
        let body = if passed {
            json!({
                "Summary": summary,
                "EvidenceArtifactId": evidence_artifact_id,
                "MeasurementsJson": measurements_json,
            })
        } else {
            json!({
                "FailureReason": summary,
                "EvidenceArtifactId": evidence_artifact_id,
                "MeasurementsJson": measurements_json,
            })
        };
        post_directed_action(ctx, base_url, headers, "Trials", &trial_id, action, body)?;
    }
    Ok(())
}

fn maybe_record_generation_survivor(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    generation_id: &str,
    variant_id: &str,
) -> Result<(), String> {
    let variant = get_entity(ctx, base_url, headers, "Variants", variant_id)?;
    if matches!(entity_status(&variant).as_str(), "Eliminated" | "Failed") {
        return Ok(());
    }

    let filter = format!("VariantId%20eq%20'{}'", escape_odata_id(variant_id));
    let results = list_entities(ctx, base_url, headers, "StageResults", &filter)?;
    let has_unfinished = results.iter().any(|result| {
        let status = result.get("status").and_then(Value::as_str).unwrap_or("");
        matches!(status, "Pending" | "Running" | "Failed" | "Eliminated")
    });
    if !has_unfinished {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Generations",
            generation_id,
            "RecordGenerationSurvivor",
            json!({ "VariantId": variant_id }),
        )?;
    }
    Ok(())
}
