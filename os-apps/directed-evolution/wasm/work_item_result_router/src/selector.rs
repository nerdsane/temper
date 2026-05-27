fn route_selector(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    generation_id: &str,
    work_item_fields: &Value,
    output: &Value,
) -> Result<Value, String> {
    let generation = get_entity(ctx, base_url, headers, "Generations", generation_id)?;
    if matches!(entity_status(&generation).as_str(), "Completed" | "Failed") {
        return Ok(json!({
            "ignored": true,
            "reason": "generation already terminal",
            "generation_id": generation_id,
        }));
    }

    let generation_fields = state_fields(&generation);
    let episode_id = field_str(&generation_fields, &["EpisodeId"]);
    let parent_version_id = field_str(&generation_fields, &["ParentVersionId"]);
    let episode = get_entity(ctx, base_url, headers, "Episodes", &episode_id)?;
    let episode_fields = state_fields(&episode);
    let organism_id = field_str(&episode_fields, &["OrganismId"]);
    let stage_count =
        parse_json_string_array(&field_str(&episode_fields, &["EvaluationStageIdsJson"])).len();
    let outcomes = collect_generation_outcomes(ctx, base_url, headers, generation_id, stage_count)?;
    let survivor_ids = outcomes
        .iter()
        .filter(|outcome| outcome.survived)
        .map(|outcome| outcome.id.clone())
        .collect::<Vec<_>>();
    if survivor_ids.is_empty() {
        return Err(format!(
            "selector cannot promote generation {generation_id}: no surviving variants"
        ));
    }

    let winner_variant_id = select_requested_winner(output, &survivor_ids)?;
    let winner = outcomes
        .iter()
        .find(|outcome| outcome.id == winner_variant_id)
        .ok_or_else(|| format!("winner {winner_variant_id} not found in generation outcomes"))?;
    let selector_brain_run_id = field_str(work_item_fields, &["BrainRunId"]);
    let selection_pressure_id = field_str(&episode_fields, &["SelectionPressureId"]);
    let selection_explanation = nonempty(
        lookup_string_deep(
            output,
            &[
                "selection_explanation",
                "SelectionExplanation",
                "summary",
                "reason",
            ],
        ),
        format!(
            "Selector brain chose {} from {} surviving variant(s).",
            winner_variant_id,
            survivor_ids.len()
        ),
    );
    let app_ref = nonempty(
        lookup_string_deep(output, &["app_ref", "appRef", "AppRef"]),
        winner.app_ref.clone(),
    );
    let commit_ref = nonempty(
        lookup_string_deep(
            output,
            &["commit_ref", "commitRef", "branch_ref", "branchRef"],
        ),
        winner.branch_ref.clone(),
    );

    let evidence_artifact_id = create_entity(ctx, base_url, headers, "EvidenceArtifacts")?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "EvidenceArtifacts",
        &evidence_artifact_id,
        "RecordEvidenceArtifact",
        json!({
            "ArtifactKind": "selection",
            "Uri": nonempty(lookup_string_deep(output, &["evidence_uri", "evidenceRef", "diff_ref", "diffRef"]), app_ref.clone()),
            "Summary": selection_explanation,
            "CorrelationJson": output.to_string(),
            "Digest": lookup_string_deep(output, &["digest", "Digest"]),
        }),
    )?;

    ensure_generation_selection_started(ctx, base_url, headers, &generation, generation_id)?;
    ensure_episode_selection_started(ctx, base_url, headers, &episode, &episode_id, generation_id)?;

    let winner_entity = get_entity(ctx, base_url, headers, "Variants", &winner_variant_id)?;
    match entity_status(&winner_entity).as_str() {
        "Active" => {
            post_directed_action(
                ctx,
                base_url,
                headers,
                "Variants",
                &winner_variant_id,
                "SelectVariant",
                json!({
                    "SelectionPressureId": selection_pressure_id,
                    "SelectorBrainRunId": selector_brain_run_id,
                    "EvidenceArtifactId": evidence_artifact_id,
                    "Reason": selection_explanation,
                }),
            )?;
        }
        "Selected" | "Promoted" => {}
        status => {
            return Err(format!(
                "selector chose variant {winner_variant_id} in non-selectable state {status}"
            ));
        }
    }

    eliminate_non_winning_survivors(
        ctx,
        base_url,
        headers,
        &outcomes,
        &winner_variant_id,
        &evidence_artifact_id,
        &selection_explanation,
    )?;

    let refreshed_generation = get_entity(ctx, base_url, headers, "Generations", generation_id)?;
    if entity_status(&refreshed_generation) == "Selecting" {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Generations",
            generation_id,
            "CompleteGeneration",
            json!({
                "WinnerVariantId": winner_variant_id,
                "Summary": selection_explanation,
            }),
        )?;
    }

    let refreshed_episode = get_entity(ctx, base_url, headers, "Episodes", &episode_id)?;
    if entity_status(&refreshed_episode) == "Selecting" {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Episodes",
            &episode_id,
            "RecordEpisodeWinner",
            json!({
                "WinningVariantId": winner_variant_id,
                "SelectorBrainRunId": selector_brain_run_id,
                "SelectionExplanation": selection_explanation,
                "EvidenceArtifactId": evidence_artifact_id,
            }),
        )?;
    }

    let promotion_id = create_entity(ctx, base_url, headers, "Promotions")?;
    let new_organism_version_id = create_entity(ctx, base_url, headers, "OrganismVersions")?;
    let lineage_edge_id = create_entity(ctx, base_url, headers, "LineageEdges")?;

    if !parent_version_id.is_empty() {
        let parent = get_entity(
            ctx,
            base_url,
            headers,
            "OrganismVersions",
            &parent_version_id,
        )?;
        if entity_status(&parent) == "Parent" {
            post_directed_action(
                ctx,
                base_url,
                headers,
                "OrganismVersions",
                &parent_version_id,
                "SupersedeOrganismVersion",
                json!({
                    "NewParentVersionId": new_organism_version_id,
                    "PromotionId": promotion_id,
                }),
            )?;
        }
    }

    post_directed_action(
        ctx,
        base_url,
        headers,
        "OrganismVersions",
        &new_organism_version_id,
        "MarkOrganismVersionParent",
        json!({
            "OrganismId": organism_id,
            "AppRef": app_ref,
            "CommitRef": commit_ref,
            "PromotionId": promotion_id,
            "Summary": selection_explanation,
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Promotions",
        &promotion_id,
        "PromoteWinner",
        json!({
            "EpisodeId": episode_id,
            "WinningVariantId": winner_variant_id,
            "ParentVersionId": parent_version_id,
            "NewOrganismVersionId": new_organism_version_id,
            "SelectionExplanation": selection_explanation,
            "EvidenceArtifactId": evidence_artifact_id,
            "AppRef": app_ref,
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Variants",
        &winner_variant_id,
        "PromoteVariant",
        json!({
            "PromotionId": promotion_id,
            "OrganismVersionId": new_organism_version_id,
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Organisms",
        &organism_id,
        "RecordOrganismVersion",
        json!({
            "OrganismVersionId": new_organism_version_id,
            "PromotionId": promotion_id,
            "AppRef": app_ref,
            "Summary": selection_explanation,
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "LineageEdges",
        &lineage_edge_id,
        "RecordLineageEdge",
        json!({
            "OrganismId": organism_id,
            "ParentVersionId": parent_version_id,
            "ChildVersionId": new_organism_version_id,
            "PromotionId": promotion_id,
            "MutationSummary": winner.summary,
            "EvidenceArtifactId": evidence_artifact_id,
        }),
    )?;
    link_evidence(
        ctx,
        base_url,
        headers,
        &evidence_artifact_id,
        "Variant",
        &winner_variant_id,
    )?;
    link_evidence(
        ctx,
        base_url,
        headers,
        &evidence_artifact_id,
        "Promotion",
        &promotion_id,
    )?;
    let promoter_work_item_id = queue_promotion_materialization_work_item(
        ctx,
        base_url,
        headers,
        &promotion_id,
        &episode_id,
        &winner_variant_id,
        &organism_id,
        &app_ref,
        &winner.branch_ref,
    )?;

    Ok(json!({
        "routed": "selector",
        "generation_id": generation_id,
        "winner_variant_id": winner_variant_id,
        "promotion_id": promotion_id,
        "promoter_work_item_id": promoter_work_item_id,
        "organism_version_id": new_organism_version_id,
        "lineage_edge_id": lineage_edge_id,
        "evidence_artifact_id": evidence_artifact_id,
    }))
}

fn eliminate_non_winning_survivors(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    outcomes: &[VariantOutcome],
    winner_variant_id: &str,
    evidence_artifact_id: &str,
    selection_explanation: &str,
) -> Result<(), String> {
    for outcome in outcomes {
        if outcome.id == winner_variant_id || !outcome.survived || outcome.status != "Active" {
            continue;
        }

        post_directed_action(
            ctx,
            base_url,
            headers,
            "Variants",
            &outcome.id,
            "EliminateVariant",
            json!({
                "EliminationRuleId": "selection-not-winner",
                "StageResultId": "",
                "EvidenceArtifactId": evidence_artifact_id,
                "Reason": format!(
                    "Selection completed with {winner_variant_id} as the winning variant. This survivor was eliminated at the selection boundary because it was not chosen: {selection_explanation}"
                ),
            }),
        )?;
    }
    Ok(())
}

fn select_requested_winner(output: &Value, survivor_ids: &[String]) -> Result<String, String> {
    let requested_winner = lookup_string_deep(
        output,
        &[
            "winning_variant_id",
            "winner_variant_id",
            "WinningVariantId",
            "VariantId",
        ],
    );
    if requested_winner.trim().is_empty() {
        return Err("selector output did not include winning_variant_id".to_string());
    }
    if !survivor_ids.contains(&requested_winner) {
        return Err(format!(
            "selector chose {requested_winner}, which is not in surviving variants: {}",
            survivor_ids.join(",")
        ));
    }
    Ok(requested_winner)
}

fn queue_promotion_materialization_work_item(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    promotion_id: &str,
    episode_id: &str,
    winning_variant_id: &str,
    organism_id: &str,
    app_ref: &str,
    branch_ref: &str,
) -> Result<String, String> {
    let work_item_id = create_entity(ctx, base_url, headers, "WorkItems")?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "WorkItems",
        &work_item_id,
        "QueueWorkItem",
        json!({
            "Role": "promoter",
            "TargetEntityType": "Promotion",
            "TargetEntityId": promotion_id,
            "PromptRef": format!(
                "literal:{}",
                promoter_prompt(
                    promotion_id,
                    episode_id,
                    winning_variant_id,
                    organism_id,
                    app_ref,
                    branch_ref,
                )
            ),
            "ContextRef": format!("promotion:{promotion_id}"),
            "OutputSchemaRef": "directed-evolution.promoter.v1",
            "CorrelationJson": json!({
                "promotion_id": promotion_id,
                "episode_id": episode_id,
                "winning_variant_id": winning_variant_id,
                "organism_id": organism_id,
                "app_ref": app_ref,
                "branch_ref": branch_ref,
            }).to_string(),
        }),
    )?;
    Ok(work_item_id)
}
