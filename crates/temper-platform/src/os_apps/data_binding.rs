//! Artifact-carried module SDK binding verification.

use temper_wasm_sdk::data::{
    ArtifactModuleSdkBinding, ModuleDataBudgets, ModuleDataGrant, ModuleSdkManifest,
    read_module_sdk_artifact_binding,
};

pub(super) fn verify_module_data_binding(
    wasm: &[u8],
    module_name: &str,
    declared_grant: &ModuleDataGrant,
    supplied: &ModuleSdkManifest,
    regenerated: &ModuleSdkManifest,
) -> Result<ModuleSdkManifest, String> {
    supplied.verify_binding()?;
    if supplied.module_name != module_name {
        return Err("module data binding name mismatch".into());
    }
    let embedded = read_module_sdk_artifact_binding(wasm)?
        .ok_or_else(|| "module artifact has no SDK binding custom section".to_string())?;
    let expected_embedded = ArtifactModuleSdkBinding::from_manifest(supplied)?;
    if embedded != expected_embedded {
        return Err("module SDK sidecar is not carried by the loaded artifact".into());
    }

    let mut supplied_without_proof = supplied.clone();
    supplied_without_proof.compatibility_proof = None;
    let mut regenerated_without_proof = regenerated.clone();
    regenerated_without_proof.compatibility_proof = None;
    if supplied_without_proof == regenerated_without_proof {
        if &regenerated.grant != declared_grant {
            return Err("module data binding grant mismatch".into());
        }
        return Ok(regenerated.clone());
    }

    let proof = supplied
        .compatibility_proof
        .as_ref()
        .ok_or_else(|| "module data binding differs without an artifact-bound proof".to_string())?;
    let prior_hashes = supplied.used_symbol_hashes()?;
    let candidate_hashes = regenerated.used_symbol_hashes()?;
    if proof.prior_closure_digest != supplied.closure_digest
        || proof.candidate_closure_digest != regenerated.closure_digest
        || proof.prior_grant_digest != supplied.grant_digest
        || proof.candidate_grant_digest != regenerated.grant_digest
        || proof.prior_used_symbol_hashes != prior_hashes
        || proof.candidate_used_symbol_hashes != candidate_hashes
    {
        return Err("module data compatibility proof failed host recomputation".into());
    }
    if prior_hashes
        .iter()
        .any(|(symbol, hash)| candidate_hashes.get(symbol) != Some(hash))
    {
        return Err("module data compatibility proof changes a used symbol".into());
    }
    if !grant_is_equal_or_narrower(&regenerated.grant, &supplied.grant)
        || &regenerated.grant != declared_grant
    {
        return Err("module data compatibility proof widens or mismatches the grant".into());
    }
    let mut activated = supplied.clone();
    activated.grant = regenerated.grant.clone();
    activated.grant_digest = regenerated.grant_digest.clone();
    activated.compatibility_proof = None;
    Ok(activated)
}

fn grant_is_equal_or_narrower(candidate: &ModuleDataGrant, prior: &ModuleDataGrant) -> bool {
    if !candidate.operations.is_subset(&prior.operations)
        || !budgets_are_equal_or_narrower(&candidate.budgets, &prior.budgets)
    {
        return false;
    }
    candidate.entities.iter().all(|entity| {
        prior
            .entities
            .iter()
            .find(|prior| prior.entity_type == entity.entity_type)
            .is_some_and(|prior| {
                entity.actions.is_subset(&prior.actions)
                    && entity.composite_actions.is_subset(&prior.composite_actions)
                    && entity
                        .query_filter_fields
                        .is_subset(&prior.query_filter_fields)
                    && entity
                        .query_order_fields
                        .is_subset(&prior.query_order_fields)
                    && entity.file_operations.is_subset(&prior.file_operations)
            })
    })
}

fn budgets_are_equal_or_narrower(candidate: &ModuleDataBudgets, prior: &ModuleDataBudgets) -> bool {
    candidate.max_calls <= prior.max_calls
        && candidate.max_batch_items <= prior.max_batch_items
        && candidate.max_page_items <= prior.max_page_items
        && candidate.max_request_bytes <= prior.max_request_bytes
        && candidate.max_response_bytes <= prior.max_response_bytes
        && candidate.max_open_responses <= prior.max_open_responses
        && candidate.max_open_streams <= prior.max_open_streams
        && candidate.max_stream_bytes <= prior.max_stream_bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::csdl::parse_csdl;
    use temper_wasm_sdk::data::{DataOperationKind, EntityDataGrant};

    const CSDL: &str = r#"<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Temper.App" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Task"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType><EntityContainer Name="Container"><EntitySet Name="Tasks" EntityType="Temper.App.Task"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;

    fn grant() -> ModuleDataGrant {
        ModuleDataGrant {
            operations: [DataOperationKind::EntityGet].into_iter().collect(),
            entities: vec![EntityDataGrant {
                entity_type: "Temper.App.Task".into(),
                ..EntityDataGrant::default()
            }],
            ..ModuleDataGrant::default()
        }
    }

    #[test]
    fn exact_binding_must_be_carried_by_loaded_artifact() {
        let csdl = parse_csdl(CSDL).unwrap();
        let generated =
            temper_codegen::generate_module_sdk(&csdl, "worker", "closure", "closure", "", grant())
                .unwrap();
        let packaged =
            temper_codegen::package_generated_module_sdk(b"\0asm\x01\0\0\0", generated).unwrap();
        let regenerated = temper_codegen::generate_module_sdk(
            &csdl,
            "worker",
            "closure",
            "closure",
            &packaged.manifest.artifact_digest,
            grant(),
        )
        .unwrap();
        assert!(
            verify_module_data_binding(
                &packaged.wasm,
                "worker",
                &grant(),
                &packaged.manifest,
                &regenerated.manifest,
            )
            .is_ok()
        );
        assert!(
            verify_module_data_binding(
                b"\0asm\x01\0\0\0",
                "worker",
                &grant(),
                &packaged.manifest,
                &regenerated.manifest,
            )
            .is_err()
        );
    }
}
