#![allow(dead_code)]

include!("../../common.rs");
include!("contract.rs");
include!("actions.rs");

temper_side_effect_module! {
    fn run(ctx: Context) -> Result<Value> {
        if ctx.trigger_action != "SubmitEpisodeStartRequest" {
            return Err(format!(
                "episode_start_requestor: unsupported trigger action {}",
                ctx.trigger_action
            ));
        }

        let request_id = entity_id(&ctx);
        let fields = fields(&ctx);
        let base_url = resolve_api_url(&ctx);
        let headers = odata_headers(&ctx);

        match materialize_episode_start_request(&ctx, &base_url, &headers, &request_id, &fields) {
            Ok(result) => Ok(result),
            Err(error) => {
                let _ = post_directed_action(
                    &ctx,
                    &base_url,
                    &headers,
                    "EpisodeStartRequests",
                    &request_id,
                    "FailEpisodeStartRequest",
                    json!({
                        "FailureReason": error,
                        "EvidenceArtifactId": "",
                    }),
                );
                Ok(json!({
                    "started": false,
                    "request_id": request_id,
                    "error": error,
                }))
            }
        }
    }
}

fn materialize_episode_start_request(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    request_id: &str,
    fields: &Value,
) -> Result<Value, String> {
    let direction_id = required(
        field_str(fields, &["DirectionId"]),
        "EpisodeStartRequest.DirectionId",
    )?;
    let direction = get_entity(ctx, base_url, headers, "Directions", &direction_id)?;
    let direction_fields = state_fields(&direction);
    let organism_id = required(
        nonempty(
            field_str(fields, &["OrganismId"]),
            field_str(&direction_fields, &["OrganismId"]),
        ),
        "EpisodeStartRequest.OrganismId",
    )?;
    let organism = get_entity(ctx, base_url, headers, "Organisms", &organism_id)?;
    let organism_fields = state_fields(&organism);
    let parent_version_id = required(
        nonempty(
            field_str(fields, &["ParentVersionId"]),
            nonempty(
                field_str(&organism_fields, &["ParentVersionId"]),
                field_str(&organism_fields, &["OrganismVersionId"]),
            ),
        ),
        "EpisodeStartRequest.ParentVersionId",
    )?;
    let autonomy_lane = nonempty(
        field_str(fields, &["AutonomyLane"]),
        nonempty(
            field_str(&direction_fields, &["AutonomyLane"]),
            "growth-human-gated".to_string(),
        ),
    );
    let requested_by = nonempty(
        field_str(fields, &["RequestedBy"]),
        "codex-chat".to_string(),
    );
    let started_by = nonempty(field_str(fields, &["StartedBy"]), requested_by.clone());
    let adaptation_goal = required(
        nonempty(
            field_str(fields, &["AdaptationGoal"]),
            field_str(&direction_fields, &["ProposedAdaptationGoal"]),
        ),
        "EpisodeStartRequest.AdaptationGoal",
    )?;

    let contract = EpisodeStartContract {
        direction_id,
        organism_id,
        parent_version_id,
        autonomy_lane,
        requested_by,
        started_by,
        adaptation_goal,
        human_notes: field_str(fields, &["HumanNotes"]),
        reason: nonempty(
            field_str(fields, &["Reason"]),
            "Start the negotiated Directed Evolution episode.".to_string(),
        ),
        selection_statement: nonempty(
            field_str(fields, &["SelectionStatement"]),
            "Select the variant that best satisfies the Adaptation Goal while preserving all Viability Constraints.".to_string(),
        ),
        proposed_constraints_json: field_str(&direction_fields, &["ProposedViabilityConstraintsJson"]),
        contract_json: field_str(fields, &["ContractJson"]),
        metrics: metric_plans(fields),
        constraints: constraint_plans(fields, &direction_fields),
        elimination_rules: elimination_rule_plans(fields),
        scoring_rules: scoring_rule_plans(fields),
        stages: evaluation_stage_plans(fields),
    }
    .with_defaults();

    contract.validate()?;
    start_episode_from_contract(ctx, base_url, headers, request_id, contract)
}
