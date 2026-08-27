//! Exact artifact-carried generated-client binding verification.

use super::*;

fn binding_matches_regenerated(
    wasm: &[u8],
    module_name: &str,
    supplied: &temper_wasm_sdk::data::ModuleSdkManifest,
    regenerated: &temper_wasm_sdk::data::ModuleSdkManifest,
) -> Result<(), String> {
    use temper_wasm_sdk::data::{ArtifactModuleSdkBinding, read_module_sdk_artifact_binding};

    supplied.verify_binding()?;
    if supplied.module_name != module_name {
        return Err("module data binding name mismatch".into());
    }
    let embedded = read_module_sdk_artifact_binding(wasm)?
        .ok_or_else(|| "module artifact has no SDK binding custom section".to_string())?;
    if embedded != ArtifactModuleSdkBinding::from_manifest(supplied)? {
        return Err("module SDK sidecar is not carried by the loaded artifact".into());
    }
    let mut supplied_without_proof = supplied.clone();
    supplied_without_proof.compatibility_proof = None;
    let mut regenerated_without_proof = regenerated.clone();
    regenerated_without_proof.compatibility_proof = None;
    if supplied_without_proof == regenerated_without_proof {
        return Ok(());
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
        || prior_hashes
            .iter()
            .any(|(symbol, hash)| candidate_hashes.get(symbol) != Some(hash))
        || regenerated.grant != supplied.grant
    {
        return Err("module data compatibility proof failed host recomputation".into());
    }
    Ok(())
}

pub(super) async fn verify_scoped_module_data_bindings(
    state: &ServerState,
    record: &SchemaDeploymentRecord,
) -> Result<bool, ServiceError> {
    if record.bundle.wasm_module_data_bindings.is_empty() {
        return Ok(true);
    }
    let csdl = temper_spec::parse_csdl(&record.bundle.canonical_csdl)
        .map_err(|error| ServiceError::new("verification_failed", error.to_string(), false))?;
    let ioa = record
        .bundle
        .canonical_ioa
        .iter()
        .map(
            |(entity_type, source)| temper_spec::bundle::IoaSourceInput {
                entity_type: entity_type.clone(),
                source: source.clone(),
            },
        )
        .collect::<Vec<_>>();
    let closure_digest = temper_spec::bundle::scoped_module_data_closure_digest(
        &record.bundle.canonical_csdl,
        ioa.clone(),
    )
    .map_err(|error| ServiceError::new("verification_failed", error.to_string(), false))?;
    let tenant = TenantId::new(&record.bundle.tenant);
    for (module_name, stored) in &record.bundle.wasm_module_data_bindings {
        let Some(artifact_digest) = record.bundle.wasm_module_digests.get(module_name) else {
            return Ok(false);
        };
        let Some(artifact_hash) = artifact_digest.strip_prefix("sha256:") else {
            return Ok(false);
        };
        let supplied: temper_wasm_sdk::data::ModuleSdkManifest =
            serde_json::from_str(&stored.canonical_manifest_json).map_err(|error| {
                ServiceError::new("verification_failed", error.to_string(), false)
            })?;
        let actual_binding_digest = supplied
            .binding_digest()
            .map(|digest| format!("sha256:{digest}"))
            .map_err(|error| ServiceError::new("verification_failed", error, false))?;
        if stored.binding_digest != actual_binding_digest
            || supplied.artifact_digest != artifact_hash
        {
            return Ok(false);
        }
        let regenerated = temper_codegen::generate_module_sdk(
            &csdl,
            &ioa,
            module_name,
            &closure_digest,
            &closure_digest,
            artifact_hash,
            supplied.grant.clone(),
        )
        .map_err(|error| ServiceError::new("verification_failed", error.to_string(), false))?;
        let wasm = state
            .load_scoped_wasm_artifact_bytes(&tenant, module_name, artifact_hash)
            .await
            .map_err(|error| ServiceError::new("backend_unavailable", error, true))?;
        if binding_matches_regenerated(&wasm, module_name, &supplied, &regenerated.manifest)
            .is_err()
        {
            return Ok(false);
        }
    }
    Ok(true)
}
