#[allow(clippy::too_many_arguments)]
fn maybe_auto_start_repair_episode(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    direction_id: &str,
    organism_id: &str,
    pressure_class: &str,
    autonomy_lane: &str,
    proposed_adaptation_goal: &str,
    proposed_constraints: &str,
    brain_run_id: &str,
    output: &Value,
) -> Result<Value, String> {
    if !repair_autostart_lane_allowed(pressure_class, autonomy_lane) {
        return Ok(json!({
            "started": false,
            "reason": "direction lane is not automatic repair",
            "autonomy_lane": autonomy_lane,
            "pressure_class": pressure_class,
        }));
    }

    let policy = active_autonomy_policy_for_organism(ctx, base_url, headers, organism_id)?;
    let Some(policy) = policy else {
        return Ok(json!({
            "started": false,
            "reason": "no active AutonomyPolicy for organism",
            "autonomy_lane": autonomy_lane,
            "pressure_class": pressure_class,
        }));
    };
    let policy_fields = state_fields(&policy);
    let policy_json = field_str(&policy_fields, &["PolicyJson"]);
    if !policy_permits_repair_autostart(&policy_json) {
        return Ok(json!({
            "started": false,
            "reason": "active AutonomyPolicy does not permit automatic repair",
            "policy_id": entity_id_from_entity(&policy),
            "autonomy_lane": autonomy_lane,
            "pressure_class": pressure_class,
        }));
    }

    let organism = get_entity(ctx, base_url, headers, "Organisms", organism_id)?;
    let organism_fields = state_fields(&organism);
    let parent_version_id = nonempty(
        field_str(&organism_fields, &["OrganismVersionId"]),
        field_str(&organism_fields, &["ParentVersionId"]),
    );
    if parent_version_id.trim().is_empty() {
        return Ok(json!({
            "started": false,
            "reason": "organism has no parent version",
            "policy_id": entity_id_from_entity(&policy),
        }));
    }

    let episode_id = create_entity(ctx, base_url, headers, "Episodes")?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Episodes",
        &episode_id,
        "BeginEpisodeNegotiation",
        json!({
            "DirectionId": direction_id,
            "OrganismId": organism_id,
            "ParentVersionId": parent_version_id,
            "AutonomyLane": autonomy_lane,
        }),
    )?;

    let goal_id = create_entity(ctx, base_url, headers, "AdaptationGoals")?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "AdaptationGoals",
        &goal_id,
        "ActivateAdaptationGoal",
        json!({
            "EpisodeId": episode_id,
            "GoalStatement": nonempty(
                proposed_adaptation_goal.to_string(),
                "Repair the observed failing behavior while preserving the current Agent Answers contract.".to_string(),
            ),
            "CreatedByBrainRunId": brain_run_id,
            "HumanNotes": "Autostarted by active repair autonomy policy after observer-brain evidence.",
        }),
    )?;

    let metric_ids =
        activate_repair_metric_definitions(ctx, base_url, headers, output, brain_run_id)?;
    let constraint_ids = activate_repair_constraints(
        ctx,
        base_url,
        headers,
        &episode_id,
        proposed_constraints,
        brain_run_id,
    )?;
    let elimination_rule_id = create_entity(ctx, base_url, headers, "EliminationRules")?;
    let metric_ids_json = json!(metric_ids).to_string();
    post_directed_action(
        ctx,
        base_url,
        headers,
        "EliminationRules",
        &elimination_rule_id,
        "ActivateEliminationRule",
        json!({
            "EpisodeId": episode_id,
            "RuleStatement": nonempty(
                lookup_string_deep(output, &["elimination_rule", "EliminationRule"]),
                "Eliminate variants that fail code/spec review, regress the baseline, or fail the AI simulated-user trial.".to_string(),
            ),
            "MetricIdsJson": metric_ids_json,
            "ThresholdJson": json!({
                "baseline_regression_count": 0,
                "simulated_user_repair_success": true,
            }).to_string(),
            "CreatedByBrainRunId": brain_run_id,
        }),
    )?;

    let scoring_rule_id = create_entity(ctx, base_url, headers, "ScoringRules")?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "ScoringRules",
        &scoring_rule_id,
        "ActivateScoringRule",
        json!({
            "EpisodeId": episode_id,
            "RuleStatement": nonempty(
                lookup_string_deep(output, &["scoring_rule", "ScoringRule"]),
                "Prefer the repair variant with strongest observed recovery and no baseline regression.".to_string(),
            ),
            "MetricIdsJson": metric_ids_json,
            "Weight": "1.0",
            "CreatedByBrainRunId": brain_run_id,
        }),
    )?;

    let selection_pressure_id = create_entity(ctx, base_url, headers, "SelectionPressures")?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "SelectionPressures",
        &selection_pressure_id,
        "ActivateSelectionPressure",
        json!({
            "EpisodeId": episode_id,
            "SelectionStatement": nonempty(
                lookup_string_deep(output, &["selection_statement", "SelectionStatement"]),
                "Select the repair that resolves the observed failure while preserving existing Agent Answers behavior.".to_string(),
            ),
            "MetricIdsJson": metric_ids_json,
            "EliminationRuleIdsJson": json!([elimination_rule_id]).to_string(),
            "ScoringRuleIdsJson": json!([scoring_rule_id]).to_string(),
            "CreatedByBrainRunId": brain_run_id,
        }),
    )?;

    let stage_ids = activate_repair_evaluation_stages(
        ctx,
        base_url,
        headers,
        &episode_id,
        output,
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Episodes",
        &episode_id,
        "RecordEpisodeContract",
        json!({
            "AdaptationGoalId": goal_id,
            "SelectionPressureId": selection_pressure_id,
            "ViabilityConstraintIdsJson": json!(constraint_ids).to_string(),
            "EvaluationStageIdsJson": json!(stage_ids).to_string(),
            "EliminationRuleIdsJson": json!([elimination_rule_id]).to_string(),
            "ScoringRuleIdsJson": json!([scoring_rule_id]).to_string(),
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Directions",
        direction_id,
        "SelectDirection",
        json!({
            "EpisodeId": episode_id,
            "SelectedBy": "autonomy-policy",
            "SelectionNotes": "Autostarted because observer brain classified this as bounded repair and the active AutonomyPolicy permits automatic repair.",
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Episodes",
        &episode_id,
        "StartEpisode",
        json!({
            "StartedBy": "autonomy-policy",
            "Reason": "Repair pressure is authorized for automatic Directed Evolution by the active AutonomyPolicy.",
        }),
    )?;

    Ok(json!({
        "started": true,
        "episode_id": episode_id,
        "policy_id": entity_id_from_entity(&policy),
        "autonomy_lane": autonomy_lane,
        "pressure_class": pressure_class,
    }))
}
