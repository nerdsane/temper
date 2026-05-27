struct PromotionMaterializationRecord {
    canonical_app_ref: String,
    production_tenant: String,
    runtime_ref: String,
    summary: String,
    evidence_uri: String,
    digest: String,
}

fn route_promoter(
    ctx: &Context,
    base_url: &str,
    headers: &[(String, String)],
    promotion_id: &str,
    _work_item_fields: &Value,
    output: &Value,
) -> Result<Value, String> {
    let promotion = get_entity(ctx, base_url, headers, "Promotions", promotion_id)?;
    if entity_status(&promotion) != "Promoted" {
        return Ok(json!({
            "ignored": true,
            "reason": "promotion is not promoted",
            "promotion_id": promotion_id,
            "status": entity_status(&promotion),
        }));
    }
    let promotion_fields = state_fields(&promotion);
    let fallback_app_ref = field_str(&promotion_fields, &["AppRef"]);
    let record = promotion_materialization_record(output, &fallback_app_ref);

    let evidence_artifact_id = create_entity(ctx, base_url, headers, "EvidenceArtifacts")?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "EvidenceArtifacts",
        &evidence_artifact_id,
        "RecordEvidenceArtifact",
        json!({
            "ArtifactKind": "promotion_materialization",
            "Uri": nonempty(record.evidence_uri.clone(), record.runtime_ref.clone()),
            "Summary": record.summary.clone(),
            "CorrelationJson": output.to_string(),
            "Digest": record.digest.clone(),
        }),
    )?;
    post_directed_action(
        ctx,
        base_url,
        headers,
        "Promotions",
        promotion_id,
        "RecordPromotionMaterialization",
        json!({
            "CanonicalAppRef": record.canonical_app_ref.clone(),
            "ProductionTenant": record.production_tenant.clone(),
            "RuntimeRef": record.runtime_ref.clone(),
            "EvidenceArtifactId": evidence_artifact_id,
            "Summary": record.summary.clone(),
        }),
    )?;
    link_evidence(
        ctx,
        base_url,
        headers,
        &evidence_artifact_id,
        "Promotion",
        promotion_id,
    )?;

    let episode_id = field_str(&promotion_fields, &["EpisodeId"]);
    let organism_version_id = field_str(&promotion_fields, &["NewOrganismVersionId"]);
    if !episode_id.trim().is_empty() {
        let episode = get_entity(ctx, base_url, headers, "Episodes", &episode_id)?;
        if entity_status(&episode) == "Promoting" {
            post_directed_action(
                ctx,
                base_url,
                headers,
                "Episodes",
                &episode_id,
                "CompleteEpisode",
                json!({
                    "PromotionId": promotion_id,
                    "OrganismVersionId": organism_version_id,
                    "Summary": record.summary.clone(),
                }),
            )?;
        }
    }

    Ok(json!({
        "routed": "promoter",
        "promotion_id": promotion_id,
        "evidence_artifact_id": evidence_artifact_id,
        "canonical_app_ref": record.canonical_app_ref,
        "production_tenant": record.production_tenant,
        "runtime_ref": record.runtime_ref,
    }))
}

fn promotion_materialization_record(
    output: &Value,
    fallback_app_ref: &str,
) -> PromotionMaterializationRecord {
    let canonical_app_ref = nonempty(
        lookup_string_deep(
            output,
            &[
                "canonical_app_ref",
                "canonicalAppRef",
                "app_ref",
                "appRef",
                "AppRef",
            ],
        ),
        fallback_app_ref.to_string(),
    );
    let production_tenant = nonempty(
        lookup_string_deep(
            output,
            &[
                "production_tenant",
                "productionTenant",
                "target_tenant",
                "targetTenant",
                "TargetTenant",
            ],
        ),
        "default".to_string(),
    );
    let runtime_ref = nonempty(
        lookup_string_deep(output, &["runtime_ref", "runtimeRef", "RuntimeRef"]),
        format!("temper://tenant/{production_tenant}/app/{canonical_app_ref}"),
    );
    let summary = nonempty(
        lookup_string_deep(output, &["summary", "Summary", "materialization_summary"]),
        format!("Materialized {canonical_app_ref} into tenant {production_tenant}."),
    );
    let evidence_uri = lookup_value_deep(output, &["evidence_refs", "evidenceRefs"])
        .and_then(|value| value.as_array().and_then(|items| items.first().cloned()))
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| {
            lookup_string_deep(
                output,
                &["evidence_uri", "evidenceUri", "EvidenceUri", "runtime_ref"],
            )
        });
    let digest = lookup_string_deep(output, &["digest", "Digest"]);

    PromotionMaterializationRecord {
        canonical_app_ref,
        production_tenant,
        runtime_ref,
        summary,
        evidence_uri,
        digest,
    }
}

fn promoter_prompt(
    promotion_id: &str,
    episode_id: &str,
    winning_variant_id: &str,
    organism_id: &str,
    app_ref: &str,
    branch_ref: &str,
) -> String {
    format!(
        "Materialize Directed Evolution promotion.\n\
PromotionId: {promotion_id}\n\
EpisodeId: {episode_id}\n\
WinningVariantId: {winning_variant_id}\n\
OrganismId: {organism_id}\n\
AppRef: {app_ref}\n\
BranchRef: {branch_ref}\n\n\
Execute the canonical promotion side effects: push the winning commit to the canonical Genesis ref, \
publish the app version, hot-load the pinned app into the production tenant, and return JSON with \
canonical_app_ref, production_tenant, runtime_ref, summary, evidence_refs, and digest."
    )
}
