fn start_episode_from_contract(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    request_id: &str,
    contract: EpisodeStartContract,
) -> Result<Value, String> {
    let episode_id = create_entity(ctx, base_url, headers, "Episodes")?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Episodes",
        &episode_id,
        "BeginEpisodeNegotiation",
        json!({
            "DirectionId": contract.direction_id,
            "OrganismId": contract.organism_id,
            "ParentVersionId": contract.parent_version_id,
            "AutonomyLane": contract.autonomy_lane,
        }),
    )?;

    let metric_ids = activate_metrics(ctx, base_url, headers, &contract.metrics)?;
    let constraint_ids = activate_constraints(
        ctx,
        base_url,
        headers,
        &episode_id,
        &contract.constraints,
        &contract.requested_by,
    )?;
    let elimination_rule_ids = activate_elimination_rules(
        ctx,
        base_url,
        headers,
        &episode_id,
        &contract.elimination_rules,
        &metric_ids,
        &contract.requested_by,
    )?;
    let scoring_rule_ids = activate_scoring_rules(
        ctx,
        base_url,
        headers,
        &episode_id,
        &contract.scoring_rules,
        &metric_ids,
        &contract.requested_by,
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
            "SelectionStatement": contract.selection_statement,
            "MetricIdsJson": json!(metric_ids.ids()).to_string(),
            "EliminationRuleIdsJson": json!(elimination_rule_ids).to_string(),
            "ScoringRuleIdsJson": json!(scoring_rule_ids).to_string(),
            "CreatedByBrainRunId": contract.requested_by,
        }),
    )?;
    let stage_ids = activate_stages(ctx, base_url, headers, &episode_id, &contract.stages)?;
    let adaptation_goal_id = create_entity(ctx, base_url, headers, "AdaptationGoals")?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "AdaptationGoals",
        &adaptation_goal_id,
        "ActivateAdaptationGoal",
        json!({
            "EpisodeId": episode_id,
            "GoalStatement": contract.adaptation_goal,
            "CreatedByBrainRunId": contract.requested_by,
            "HumanNotes": human_notes_with_contract(&contract.human_notes, &contract.contract_json),
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Episodes",
        &episode_id,
        "RecordEpisodeContract",
        json!({
            "AdaptationGoalId": adaptation_goal_id,
            "SelectionPressureId": selection_pressure_id,
            "ViabilityConstraintIdsJson": json!(constraint_ids).to_string(),
            "EvaluationStageIdsJson": json!(stage_ids).to_string(),
            "EliminationRuleIdsJson": json!(elimination_rule_ids).to_string(),
            "ScoringRuleIdsJson": json!(scoring_rule_ids).to_string(),
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Directions",
        &contract.direction_id,
        "SelectDirection",
        json!({
            "EpisodeId": episode_id,
            "SelectedBy": contract.started_by,
            "SelectionNotes": format!("Selected through EpisodeStartRequest {request_id}."),
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
            "StartedBy": contract.started_by,
            "Reason": contract.reason,
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "EpisodeStartRequests",
        request_id,
        "MarkEpisodeStartRequestStarted",
        json!({
            "EpisodeId": episode_id,
            "Summary": "Episode materialized from negotiated human/brain contract.",
            "EvidenceArtifactId": "",
        }),
    )?;

    Ok(json!({
        "started": true,
        "request_id": request_id,
        "episode_id": episode_id,
    }))
}

fn activate_metrics(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    metrics: &[MetricPlan],
) -> Result<MetricIds, String> {
    let mut pairs = Vec::new();
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
                "Description": metric.description,
            }),
        )?;
        pairs.push((metric.name.clone(), metric_id));
    }
    Ok(MetricIds { pairs })
}

fn activate_constraints(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode_id: &str,
    constraints: &[ConstraintPlan],
    requested_by: &str,
) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    for constraint in constraints {
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
                "ConstraintStatement": constraint.statement,
                "ConstraintKind": constraint.kind,
                "CreatedByBrainRunId": requested_by,
            }),
        )?;
        ids.push(constraint_id);
    }
    Ok(ids)
}

fn activate_elimination_rules(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode_id: &str,
    rules: &[RulePlan],
    metric_ids: &MetricIds,
    requested_by: &str,
) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    for rule in rules {
        let rule_id = create_entity(ctx, base_url, headers, "EliminationRules")?;
        post_directed_action(
            ctx,
            base_url,
            headers,
            "EliminationRules",
            &rule_id,
            "ActivateEliminationRule",
            json!({
                "EpisodeId": episode_id,
                "RuleStatement": rule.statement,
                "MetricIdsJson": json!(metric_ids.ids_for_names(&rule.metric_names)).to_string(),
                "ThresholdJson": rule.threshold_json,
                "CreatedByBrainRunId": requested_by,
            }),
        )?;
        ids.push(rule_id);
    }
    Ok(ids)
}

fn activate_scoring_rules(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode_id: &str,
    rules: &[ScoringRulePlan],
    metric_ids: &MetricIds,
    requested_by: &str,
) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    for rule in rules {
        let rule_id = create_entity(ctx, base_url, headers, "ScoringRules")?;
        post_directed_action(
            ctx,
            base_url,
            headers,
            "ScoringRules",
            &rule_id,
            "ActivateScoringRule",
            json!({
                "EpisodeId": episode_id,
                "RuleStatement": rule.statement,
                "MetricIdsJson": json!(metric_ids.ids_for_names(&rule.metric_names)).to_string(),
                "Weight": rule.weight,
                "CreatedByBrainRunId": requested_by,
            }),
        )?;
        ids.push(rule_id);
    }
    Ok(ids)
}

fn activate_stages(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode_id: &str,
    stages: &[StagePlan],
) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    for (index, stage) in stages.iter().enumerate() {
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
