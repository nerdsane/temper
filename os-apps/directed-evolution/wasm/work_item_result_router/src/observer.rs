fn route_observer(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    signal_id: &str,
    work_item_fields: &Value,
    output: &Value,
) -> Result<Value, String> {
    let actionable = lookup_bool_deep(output, &["actionable", "Actionable"]).unwrap_or(true);
    if !actionable {
        let reason = nonempty(
            lookup_string_deep(output, &["rationale", "reason", "summary"]),
            "Observer brain marked the signal as not actionable.".to_string(),
        );
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Signals",
            signal_id,
            "IgnoreSignal",
            json!({ "Reason": reason }),
        )?;
        return Ok(json!({
            "routed": "observer",
            "signal_id": signal_id,
            "actionable": false,
        }));
    }

    let signal = get_entity(ctx, base_url, headers, "Signals", signal_id)?;
    let signal_fields = state_fields(&signal);
    let organism_id = field_str(&signal_fields, &["OrganismId"]);
    let signal_summary = field_str(&signal_fields, &["Summary"]);
    let signal_evidence_artifact_id = field_str(&signal_fields, &["EvidenceArtifactId"]);
    let brain_run_id = field_str(work_item_fields, &["BrainRunId"]);
    let pressure_class = nonempty(
        lookup_string_deep(output, &["pressure_class", "PressureClass"]),
        field_str(&signal_fields, &["SignalKind"]),
    );
    let pressure_summary = nonempty(
        lookup_string_deep(output, &["pressure_summary", "summary", "rationale"]),
        signal_summary,
    );
    let title = nonempty(
        lookup_string_deep(output, &["title", "Title"]),
        format!("Evolve for {pressure_class}"),
    );
    let direction_summary = nonempty(
        lookup_string_deep(
            output,
            &["direction_summary", "DirectionSummary", "proposal"],
        ),
        pressure_summary.clone(),
    );
    let autonomy_lane = nonempty(
        lookup_string_deep(output, &["autonomy_lane", "AutonomyLane"]),
        if pressure_class.to_ascii_lowercase().contains("repair") {
            "repair-auto".to_string()
        } else {
            "human-approval".to_string()
        },
    );
    let proposed_adaptation_goal = nonempty(
        lookup_string_deep(
            output,
            &[
                "proposed_adaptation_goal",
                "ProposedAdaptationGoal",
                "adaptation_goal",
            ],
        ),
        direction_summary.clone(),
    );
    let proposed_constraints =
        lookup_value_deep(output, &["proposed_viability_constraints", "constraints"])
            .unwrap_or_else(|| json!([]))
            .to_string();

    let pressure_id = create_entity(ctx, base_url, headers, "Pressures")?;
    let direction_id = create_entity(ctx, base_url, headers, "Directions")?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Pressures",
        &pressure_id,
        "InferPressure",
        json!({
            "OrganismId": organism_id,
            "PressureClass": pressure_class,
            "Summary": pressure_summary,
            "SignalIdsJson": json!([signal_id]).to_string(),
            "EvidenceArtifactId": signal_evidence_artifact_id,
            "BrainRunId": brain_run_id,
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Directions",
        &direction_id,
        "ProposeDirection",
        json!({
            "OrganismId": organism_id,
            "PressureIdsJson": json!([pressure_id]).to_string(),
            "PressureClass": pressure_class,
            "Title": title,
            "Summary": direction_summary,
            "ProvenanceJson": json!({
                "signal_id": signal_id,
                "pressure_id": pressure_id,
                "observer_output": output,
            }).to_string(),
            "AutonomyLane": autonomy_lane,
            "ProposedAdaptationGoal": proposed_adaptation_goal,
            "ProposedViabilityConstraintsJson": proposed_constraints,
            "BrainRunId": brain_run_id,
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Signals",
        signal_id,
        "LinkSignalToPressure",
        json!({ "PressureId": pressure_id }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Pressures",
        &pressure_id,
        "FramePressureAsDirection",
        json!({ "DirectionId": direction_id }),
    )?;

    Ok(json!({
        "routed": "observer",
        "signal_id": signal_id,
        "pressure_id": pressure_id,
        "direction_id": direction_id,
        "actionable": true,
    }))
}
