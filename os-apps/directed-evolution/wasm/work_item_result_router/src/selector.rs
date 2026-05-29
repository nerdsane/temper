#[derive(Default)]
struct MutationDetails {
    summary: String,
    changed_files_json: String,
    diff_patch: String,
}

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
    let mutation = winner_mutation_details(ctx, base_url, headers, &winner_variant_id)?;

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

    for survivor_id in survivor_ids.iter().filter(|id| *id != &winner_variant_id) {
        let survivor = get_entity(ctx, base_url, headers, "Variants", survivor_id)?;
        if entity_status(&survivor) == "Active" {
            post_directed_action(
                ctx,
                base_url,
                headers,
                "Variants",
                survivor_id,
                "RecordVariantNotSelected",
                json!({
                    "SelectionPressureId": selection_pressure_id,
                    "SelectorBrainRunId": selector_brain_run_id,
                    "EvidenceArtifactId": evidence_artifact_id,
                    "Reason": format!(
                        "Survived all declared evaluation stages, but was not selected because the winner {} had stronger evidence: {}",
                        winner_variant_id,
                        selection_explanation
                    ),
                }),
            )?;
        }
    }

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
            "EpisodeId": episode_id,
            "VariantId": winner_variant_id,
            "PromotionId": promotion_id,
            "MutationSummary": nonempty(mutation.summary.clone(), winner.summary.clone()),
            "ChangedFilesJson": mutation.changed_files_json,
            "DiffPatch": mutation.diff_patch,
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

    let promoting_episode = get_entity(ctx, base_url, headers, "Episodes", &episode_id)?;
    if entity_status(&promoting_episode) == "Promoting" {
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Episodes",
            &episode_id,
            "CompleteEpisode",
            json!({
                "PromotionId": promotion_id,
                "OrganismVersionId": new_organism_version_id,
                "Summary": selection_explanation,
            }),
        )?;
    }

    let promoter_work_item_id = queue_promoter_if_absent(
        ctx,
        base_url,
        headers,
        &episode_id,
        &promotion_id,
        &winner_variant_id,
        &app_ref,
        &organism_id,
        &parent_version_id,
        &new_organism_version_id,
    )?;

    Ok(json!({
        "routed": "selector",
        "generation_id": generation_id,
        "winner_variant_id": winner_variant_id,
        "promotion_id": promotion_id,
        "organism_version_id": new_organism_version_id,
        "lineage_edge_id": lineage_edge_id,
        "evidence_artifact_id": evidence_artifact_id,
        "promoter_work_item_id": promoter_work_item_id,
    }))
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

fn winner_mutation_details(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    variant_id: &str,
) -> Result<MutationDetails, String> {
    let filter = format!("VariantId%20eq%20'{}'", escape_odata_id(variant_id));
    let mutations = list_entities(ctx, base_url, headers, "Mutations", &filter)?;
    let Some(mutation) = mutations.first() else {
        return Ok(MutationDetails::default());
    };
    let fields = state_fields(mutation);
    Ok(MutationDetails {
        summary: field_str(&fields, &["Summary"]),
        changed_files_json: field_str(&fields, &["ChangedFilesJson"]),
        diff_patch: field_str(&fields, &["DiffPatch"]),
    })
}

fn queue_promoter_if_absent(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    episode_id: &str,
    promotion_id: &str,
    winner_variant_id: &str,
    app_ref: &str,
    organism_id: &str,
    parent_version_id: &str,
    new_organism_version_id: &str,
) -> Result<Option<String>, String> {
    let filter = format!(
        "Role%20eq%20'promoter'%20and%20TargetEntityType%20eq%20'Promotion'%20and%20TargetEntityId%20eq%20'{}'",
        escape_odata_id(promotion_id)
    );
    let existing = list_entities(ctx, base_url, headers, "WorkItems", &filter)?;
    for work_item in existing {
        let status = entity_status(&work_item);
        if matches!(
            status.as_str(),
            "Queued" | "Claimed" | "Running" | "Succeeded"
        ) {
            return Ok(Some(entity_id_from_entity(&work_item)));
        }
    }

    let work_item_id = create_entity(ctx, base_url, headers, "WorkItems")?;
    let prompt = format!(
        "Materialize Directed Evolution promotion {promotion_id}. Publish and hot-load the already-selected app ref {app_ref}; do not choose a winner or change repository files."
    );
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
            "PromptRef": format!("literal:{prompt}"),
            "ContextRef": format!("promotion:{promotion_id}"),
            "OutputSchemaRef": "directed-evolution.promotion-materialization.v1",
            "CorrelationJson": json!({
                "episode_id": episode_id,
                "promotion_id": promotion_id,
                "winner_variant_id": winner_variant_id,
                "app_ref": app_ref,
                "organism_id": organism_id,
                "parent_version_id": parent_version_id,
                "new_organism_version_id": new_organism_version_id,
            }).to_string(),
        }),
    )?;
    Ok(Some(work_item_id))
}

fn route_promoter(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    promotion_id: &str,
    work_item_fields: &Value,
    output: &Value,
) -> Result<Value, String> {
    let promotion = get_entity(ctx, base_url, headers, "Promotions", promotion_id)?;
    if entity_status(&promotion) != "Promoted" {
        return Ok(json!({
            "ignored": true,
            "reason": "promotion not in Promoted state",
            "promotion_id": promotion_id,
            "status": entity_status(&promotion),
        }));
    }

    let evidence_artifact_id = field_str(work_item_fields, &["EvidenceArtifactId"]);
    let status = lookup_string_deep(output, &["status", "Status"]);
    let succeeded = status.is_empty() || status.eq_ignore_ascii_case("succeeded");
    if succeeded {
        let canonical_app_ref = nonempty(
            lookup_string_deep(
                output,
                &["canonical_app_ref", "CanonicalAppRef", "app_ref", "AppRef"],
            ),
            field_str(&state_fields(&promotion), &["AppRef"]),
        );
        let production_tenant =
            lookup_string_deep(output, &["production_tenant", "ProductionTenant"]);
        let runtime_ref = lookup_string_deep(output, &["runtime_ref", "RuntimeRef"]);
        let summary = nonempty(
            lookup_string_deep(output, &["summary", "Summary", "reasoning_summary"]),
            format!("Materialized {canonical_app_ref} into {production_tenant}."),
        );
        post_directed_action(
            ctx,
            base_url,
            headers,
            "Promotions",
            promotion_id,
            "RecordPromotionMaterialization",
            json!({
                "CanonicalAppRef": canonical_app_ref,
                "ProductionTenant": production_tenant,
                "RuntimeRef": runtime_ref,
                "Summary": summary,
                "EvidenceArtifactId": evidence_artifact_id,
            }),
        )?;
        if !evidence_artifact_id.is_empty() {
            link_evidence(
                ctx,
                base_url,
                headers,
                &evidence_artifact_id,
                "Promotion",
                promotion_id,
            )?;
        }
        return Ok(json!({
            "routed": "promoter",
            "promotion_id": promotion_id,
            "materialized": true,
        }));
    }

    let failure_reason = nonempty(
        lookup_string_deep(output, &["failure_reason", "FailureReason", "error", "summary"]),
        "Promotion materialization failed.".to_string(),
    );
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Promotions",
        promotion_id,
        "FailPromotionMaterialization",
        json!({
            "FailureReason": failure_reason,
            "EvidenceArtifactId": evidence_artifact_id,
        }),
    )?;
    Ok(json!({
        "routed": "promoter",
        "promotion_id": promotion_id,
        "materialized": false,
    }))
}
