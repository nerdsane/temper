//! Artifact-carried module SDK binding verification.

use temper_runtime::tenant::TenantId;
use temper_wasm_sdk::data::{
    ArtifactModuleSdkBinding, ModuleDataBudgets, ModuleDataGrant, ModuleSdkManifest,
    read_module_sdk_artifact_binding,
};

use crate::app_bundles::CanonicalBundleManifestV1;
use crate::state::PlatformState;

use super::{AppBundle, WasmModuleManifest};

pub(super) fn admit_collection_specs(
    state: &PlatformState,
    tenant: &str,
    bundle: &AppBundle,
    specs_enabled: bool,
) -> Result<(), String> {
    if !specs_enabled {
        return Ok(());
    }
    let registry = state
        .registry
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for (entity_type, ioa_source) in &bundle.specs {
        let existing_source = registry
            .get_spec(&TenantId::new(tenant), entity_type)
            .map(|existing| existing.ioa_source.as_str());
        state
            .server
            .collection_workflow_mode
            .require_spec_source(existing_source, ioa_source)?;
    }
    Ok(())
}

pub(super) fn verify_bundle_data_bindings(
    bundle: &AppBundle,
    app_name: &str,
    canonical_manifest: &CanonicalBundleManifestV1,
    materialized_view: &std::path::Path,
) -> Result<std::collections::BTreeMap<String, ModuleSdkManifest>, String> {
    let mut verified = std::collections::BTreeMap::new();
    for (module_name, config) in &bundle.wasm_module_configs {
        if config.data.is_none() {
            continue;
        }
        let wasm = bundle
            .wasm_modules
            .get(module_name)
            .ok_or_else(|| format!("module '{module_name}' data binding has no WASM artifact"))?;
        let closure = crate::module_sdk_build::resolve_materialized_module(
            materialized_view,
            canonical_manifest,
            app_name,
            module_name,
        )
        .map_err(|error| format!("module data closure resolution failed: {error}"))?;
        let binding = verify_module_config_data_binding_with_csdl(
            wasm,
            module_name,
            config,
            &closure.csdl,
            &closure.ioa_sources,
            &closure.lock_digest,
        )?
        .ok_or_else(|| format!("module '{module_name}' data binding was not activated"))?;
        verified.insert(module_name.clone(), binding);
    }
    Ok(verified)
}

#[cfg(test)]
pub(super) fn verify_module_config_data_binding(
    wasm: &[u8],
    module_name: &str,
    config: &WasmModuleManifest,
    csdl_source: Option<&str>,
    ioa_sources: &[(String, String)],
    closure_id: &str,
) -> Result<Option<ModuleSdkManifest>, String> {
    if config.data.is_none() {
        return Ok(None);
    }
    let csdl_source =
        csdl_source.ok_or_else(|| "module data binding requires canonical CSDL".to_string())?;
    let csdl = temper_spec::csdl::parse_csdl(csdl_source)
        .map_err(|error| format!("module data binding CSDL is invalid: {error}"))?;
    let qualified_ioa = qualify_ioa_sources(&csdl, ioa_sources)?;
    verify_module_config_data_binding_with_csdl(
        wasm,
        module_name,
        config,
        &csdl,
        &qualified_ioa,
        closure_id,
    )
}

pub(super) fn verify_module_config_data_binding_with_csdl(
    wasm: &[u8],
    module_name: &str,
    config: &WasmModuleManifest,
    csdl: &temper_spec::csdl::CsdlDocument,
    ioa_sources: &[temper_spec::bundle::IoaSourceInput],
    closure_id: &str,
) -> Result<Option<ModuleSdkManifest>, String> {
    let Some(grant) = &config.data else {
        return Ok(None);
    };
    let binding = config
        .data_binding
        .as_ref()
        .ok_or_else(|| "module data grant requires a data_binding".to_string())?;
    let artifact_digest = temper_wasm::WasmEngine::hash_module(wasm);
    if binding.artifact_digest != artifact_digest {
        return Err("module data binding artifact digest mismatch".into());
    }
    let regenerated = temper_codegen::generate_module_sdk(
        csdl,
        ioa_sources,
        module_name,
        closure_id,
        closure_id,
        &artifact_digest,
        grant.clone(),
    )
    .map_err(|error| format!("module data binding regeneration failed: {error}"))?;
    verify_module_data_binding(wasm, module_name, grant, binding, &regenerated.manifest).map(Some)
}

pub(super) fn verify_local_module_config_data_binding(
    wasm: &[u8],
    module_name: &str,
    config: &WasmModuleManifest,
    app_dir: &std::path::Path,
    dependency_roots: &[std::path::PathBuf],
) -> Result<Option<ModuleSdkManifest>, String> {
    let resolved = crate::module_sdk_build::resolve_local_module(
        &crate::module_sdk_build::LocalModuleSdkInputs {
            app: app_dir.to_path_buf(),
            module: module_name.to_string(),
            dependency_roots: dependency_roots.to_vec(),
            app_manifest: None,
            source_out: None,
            lock: None,
        },
    )?;
    verify_module_config_data_binding_with_csdl(
        wasm,
        module_name,
        config,
        &resolved.csdl,
        &resolved.ioa_sources,
        &resolved.lock_digest,
    )
}

#[cfg(test)]
fn qualify_ioa_sources(
    csdl: &temper_spec::csdl::CsdlDocument,
    sources: &[(String, String)],
) -> Result<Vec<temper_spec::bundle::IoaSourceInput>, String> {
    let entity_types = csdl
        .schemas
        .iter()
        .flat_map(|schema| {
            schema.entity_types.iter().map(move |entity| {
                (
                    entity.name.as_str(),
                    format!("{}.{}", schema.namespace, entity.name),
                )
            })
        })
        .collect::<Vec<_>>();
    sources
        .iter()
        .map(|(short_name, source)| {
            let matches = entity_types
                .iter()
                .filter(|(candidate, _)| *candidate == short_name)
                .map(|(_, qualified)| qualified.clone())
                .collect::<Vec<_>>();
            if matches.len() != 1 {
                return Err(format!(
                    "IOA type '{short_name}' resolves to {} CSDL entity types",
                    matches.len()
                ));
            }
            Ok(temper_spec::bundle::IoaSourceInput {
                entity_type: matches[0].clone(),
                source: source.clone(),
            })
        })
        .collect()
}

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

    let prior_hashes = supplied.used_symbol_hashes()?;
    let candidate_hashes = regenerated.used_symbol_hashes()?;
    let compatible_nullability_changes =
        supplied.compatible_action_nullability_widenings(regenerated)?;
    let proof = supplied
        .compatibility_proof
        .as_ref()
        .ok_or_else(|| "module data binding differs without an artifact-bound proof".to_string())?;
    if proof.prior_closure_digest != supplied.closure_digest
        || proof.candidate_closure_digest != regenerated.closure_digest
        || proof.prior_grant_digest != supplied.grant_digest
        || proof.candidate_grant_digest != regenerated.grant_digest
        || proof.prior_used_symbol_hashes != prior_hashes
        || proof.candidate_used_symbol_hashes != candidate_hashes
    {
        return Err("module data compatibility proof failed host recomputation".into());
    }
    if prior_hashes.iter().any(|(symbol, hash)| {
        candidate_hashes.get(symbol) != Some(hash)
            && !compatible_nullability_changes.contains(symbol)
    }) {
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
                    && (!entity.query_order_by_sequence || prior.query_order_by_sequence)
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
    use temper_wasm_sdk::data::{
        DataOperationKind, EntityDataGrant, ManifestActionV1, ManifestEntityV1, ManifestPropertyV1,
        ManifestValueSourceV1, ModuleSdkCompatibilityProof, ModuleSdkMetadataDigests,
        bind_module_sdk_artifact,
    };

    const CSDL: &str = r#"<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx"><edmx:DataServices><Schema Namespace="Temper.App" xmlns="http://docs.oasis-open.org/odata/ns/edm"><EntityType Name="Task"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/><Property Name="State" Type="Edm.String" Nullable="false" DefaultValue="Open"/></EntityType><EntityContainer Name="Container"><EntitySet Name="Tasks" EntityType="Temper.App.Task"/></EntityContainer></Schema></edmx:DataServices></edmx:Edmx>"#;
    const IOA: &str = r#"[automaton]
name = "Task"
states = ["Open", "Done"]
initial = "Open"
"#;

    fn qualified_ioa() -> Vec<temper_spec::bundle::IoaSourceInput> {
        vec![temper_spec::bundle::IoaSourceInput {
            entity_type: "Temper.App.Task".into(),
            source: IOA.into(),
        }]
    }

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

    fn action_grant() -> ModuleDataGrant {
        ModuleDataGrant {
            operations: [DataOperationKind::ActionInvoke].into_iter().collect(),
            entities: vec![EntityDataGrant {
                entity_type: "Temper.App.Task".into(),
                actions: ["Close".to_string()].into_iter().collect(),
                ..EntityDataGrant::default()
            }],
            ..ModuleDataGrant::default()
        }
    }

    fn action_manifest(nullable: bool, closure: &str) -> ModuleSdkManifest {
        ModuleSdkManifest::new(
            "worker",
            ModuleSdkMetadataDigests {
                closure: closure.into(),
                dependency_lock: closure.into(),
                schema: format!("schema-{nullable}"),
            },
            "artifact",
            action_grant(),
            vec![ManifestEntityV1 {
                entity_type: "Temper.App.Task".into(),
                entity_set: "Tasks".into(),
                generated_name: "Task".into(),
                properties: Vec::new(),
                actions: vec![ManifestActionV1 {
                    canonical_name: "Close".into(),
                    generated_name: "close".into(),
                    parameters: vec![ManifestPropertyV1 {
                        canonical_name: "Reason".into(),
                        generated_name: "reason".into(),
                        type_name: "Edm.String".into(),
                        nullable,
                        source: ManifestValueSourceV1::Input,
                        default_value: None,
                        enum_members: Vec::new(),
                    }],
                    result_type: None,
                    result_enum_members: Vec::new(),
                    composite: false,
                }],
            }],
            ["Close".to_string()].into_iter().collect(),
        )
        .unwrap()
    }

    fn bound_wasm(manifest: &ModuleSdkManifest) -> Vec<u8> {
        bind_module_sdk_artifact(
            b"\0asm\x01\0\0\0",
            &ArtifactModuleSdkBinding::from_manifest(manifest).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn global_binding_gate_names_nullable_to_required_narrowing_without_a_proof() {
        let prior = action_manifest(true, "prior");
        let candidate = action_manifest(false, "candidate");
        let error = verify_module_data_binding(
            &bound_wasm(&prior),
            "worker",
            &action_grant(),
            &prior,
            &candidate,
        )
        .unwrap_err();
        assert!(error.contains("entity='Temper.App.Task'"));
        assert!(error.contains("action='Close'"));
        assert!(error.contains("parameter='Reason'"));
        assert!(error.contains("old_nullable=true new_nullable=false"));
    }

    #[test]
    fn global_binding_gate_accepts_required_to_nullable_with_valid_proof() {
        let mut prior = action_manifest(false, "prior");
        let candidate = action_manifest(true, "candidate");
        prior.compatibility_proof = Some(ModuleSdkCompatibilityProof {
            prior_closure_digest: prior.closure_digest.clone(),
            candidate_closure_digest: candidate.closure_digest.clone(),
            prior_used_symbol_hashes: prior.used_symbol_hashes().unwrap(),
            candidate_used_symbol_hashes: candidate.used_symbol_hashes().unwrap(),
            prior_grant_digest: prior.grant_digest.clone(),
            candidate_grant_digest: candidate.grant_digest.clone(),
        });
        assert!(
            verify_module_data_binding(
                &bound_wasm(&prior),
                "worker",
                &action_grant(),
                &prior,
                &candidate,
            )
            .is_ok()
        );
    }

    #[test]
    fn sequence_order_permission_cannot_widen_during_compatibility() {
        let prior = grant();
        let mut candidate = prior.clone();
        candidate.entities[0].query_order_by_sequence = true;
        assert!(!grant_is_equal_or_narrower(&candidate, &prior));

        let mut prior = prior;
        prior.entities[0].query_order_by_sequence = true;
        assert!(grant_is_equal_or_narrower(&candidate, &prior));
    }

    #[test]
    fn exact_binding_must_be_carried_by_loaded_artifact() {
        let csdl = parse_csdl(CSDL).unwrap();
        let generated = temper_codegen::generate_module_sdk(
            &csdl,
            &qualified_ioa(),
            "worker",
            "closure",
            "closure",
            "",
            grant(),
        )
        .unwrap();
        let packaged =
            temper_codegen::package_generated_module_sdk(b"\0asm\x01\0\0\0", generated).unwrap();
        let regenerated = temper_codegen::generate_module_sdk(
            &csdl,
            &qualified_ioa(),
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

        let config = WasmModuleManifest {
            name: "worker".into(),
            target: None,
            criticality: super::super::WasmModuleCriticality::Optional,
            startup_loading: super::super::WasmStartupLoading::Lazy,
            provenance: None,
            import_class: None,
            data: Some(grant()),
            data_binding: Some(packaged.manifest.clone()),
        };
        assert!(
            verify_module_config_data_binding(
                &packaged.wasm,
                "worker",
                &config,
                Some(CSDL),
                &[("Task".into(), IOA.into())],
                "different-closure",
            )
            .is_err(),
            "a closure-only change must be validated before an unchanged install can skip"
        );
    }
}
