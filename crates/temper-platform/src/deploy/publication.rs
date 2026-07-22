//! Durable publication and runtime cutover for verified deploy generations.

use std::collections::{BTreeMap, BTreeSet};

use temper_runtime::tenant::TenantId;
use temper_server::platform_store::{
    SpecPublication, SpecPublicationMode, TenantConstraintsPublication, TenantPolicyPublication,
    WasmPublication,
};
use temper_spec::csdl::CsdlDocument;
use temper_store_turso::spec_content_hash;

use super::{DeployInput, EntityDeployResult};
use crate::state::PlatformState;

async fn activate_deploy_wasm_modules(
    state: &PlatformState,
    input: &DeployInput,
) -> Result<(), String> {
    let tenant_id = TenantId::new(&input.tenant_name);
    for (module_name, wasm_bytes) in &input.wasm_modules {
        let hash = temper_wasm::WasmEngine::hash_module(wasm_bytes);
        state
            .server
            .wasm_module_registry
            .write()
            .expect("WASM module registry lock poisoned")
            .register(&tenant_id, module_name, &hash);
    }
    Ok(())
}

fn validate_deploy_wasm_modules(state: &PlatformState, input: &DeployInput) -> Result<(), String> {
    for (module_name, wasm_bytes) in &input.wasm_modules {
        state
            .server
            .wasm_engine
            .compile_and_cache(wasm_bytes)
            .map_err(|error| format!("invalid deploy WASM module '{module_name}': {error}"))?;
    }
    Ok(())
}

pub(super) async fn publish_verified_generation(
    state: &PlatformState,
    input: &DeployInput,
    entity_results: &[EntityDeployResult],
    csdl: CsdlDocument,
) -> Result<(), String> {
    validate_deploy_wasm_modules(state, input)?;
    let tenant_id = TenantId::new(&input.tenant_name);
    let ioa_pairs = entity_results
        .iter()
        .map(|result| (result.entity_name.as_str(), result.ioa_source.as_str()))
        .collect::<Vec<_>>();
    let mut publication_guard = state.server.begin_spec_publication(&tenant_id).await?;
    let preserved_constraints = state
        .registry
        .read()
        .expect("registry lock poisoned")
        .get_tenant(&tenant_id)
        .and_then(|config| config.cross_invariants_source.clone());

    let mut entity_types = BTreeSet::new();
    for entity in entity_results {
        if !entity_types.insert(entity.entity_name.as_str()) {
            return Err(format!(
                "duplicate entity type {} in deploy generation",
                entity.entity_name
            ));
        }
    }
    let mut candidate_registry = temper_server::registry::SpecRegistry::new();
    candidate_registry
        .try_register_tenant_with_reactions_constraints_and_key_epochs(
            tenant_id.clone(),
            csdl.clone(),
            input.csdl_xml.clone(),
            &ioa_pairs,
            Vec::new(),
            preserved_constraints.clone(),
            false,
            &BTreeMap::new(),
        )
        .map_err(|error| format!("invalid complete deploy generation: {error}"))?;

    let incoming_types = entity_results
        .iter()
        .map(|result| result.entity_name.as_str())
        .collect::<BTreeSet<_>>();
    let mut removed_entity_types = state
        .registry
        .read()
        .expect("registry lock poisoned")
        .entity_types(&tenant_id)
        .into_iter()
        .filter(|entity_type| !incoming_types.contains(entity_type))
        .map(str::to_string)
        .collect::<BTreeSet<_>>();

    let publications = entity_results
        .iter()
        .map(|entity| (entity, spec_content_hash(&entity.ioa_source)))
        .collect::<Vec<_>>();
    let publication_refs = publications
        .iter()
        .map(|(entity, content_hash)| SpecPublication {
            entity_type: &entity.entity_name,
            ioa_source: &entity.ioa_source,
            csdl_xml: &input.csdl_xml,
            content_hash,
        })
        .collect::<Vec<_>>();
    let wasm_publications = input
        .wasm_modules
        .iter()
        .map(|(module_name, wasm_bytes)| {
            (
                module_name,
                wasm_bytes,
                temper_wasm::WasmEngine::hash_module(wasm_bytes),
            )
        })
        .collect::<Vec<_>>();
    let wasm_publication_refs = wasm_publications
        .iter()
        .map(|(module_name, wasm_bytes, hash)| WasmPublication {
            module_name,
            wasm_bytes,
            sha256_hash: hash,
            source: "upload",
        })
        .collect::<Vec<_>>();

    let mut intent_components = vec![("csdl".to_string(), input.csdl_xml.as_bytes().to_vec())];
    intent_components.push((
        "constraints".to_string(),
        preserved_constraints
            .as_deref()
            .unwrap_or("")
            .as_bytes()
            .to_vec(),
    ));
    intent_components.extend(entity_results.iter().map(|entity| {
        (
            format!("spec:{}", entity.entity_name),
            entity.ioa_source.as_bytes().to_vec(),
        )
    }));
    intent_components.extend(
        input
            .wasm_modules
            .iter()
            .map(|(name, bytes)| (format!("wasm:{name}"), bytes.clone())),
    );
    let intent_refs = intent_components
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_slice()))
        .collect::<Vec<_>>();
    let publication_intent =
        temper_server::ServerState::spec_publication_intent("platform-deploy-replace", intent_refs);
    state
        .server
        .arm_spec_publication(&mut publication_guard, &tenant_id, &publication_intent)?;

    if let Some(store) = state
        .server
        .storage_stack
        .as_ref()
        .and_then(|stack| stack.platform.clone())
    {
        let durable_removed = store
            .publish_specs(
                &input.tenant_name,
                &publication_refs,
                SpecPublicationMode::Replace,
                TenantConstraintsPublication::Preserve,
                TenantPolicyPublication::Preserve,
                None,
                None,
                &wasm_publication_refs,
            )
            .await
            .map_err(|error| format!("durable spec publication failed: {error}"))?;
        removed_entity_types.extend(durable_removed);
    }
    let removed_entity_types = removed_entity_types.into_iter().collect::<Vec<_>>();
    let mut cutover = state
        .server
        .prepare_key_index_contracts_for_spec_activation_with_removals(
            &publication_guard,
            &tenant_id,
            &ioa_pairs,
            &removed_entity_types,
        )
        .await?;

    state
        .registry
        .write()
        .expect("registry lock poisoned")
        .try_register_tenant_with_reactions_constraints_and_key_epochs(
            tenant_id.clone(),
            csdl,
            input.csdl_xml.clone(),
            &ioa_pairs,
            Vec::new(),
            preserved_constraints,
            false,
            &cutover.activation_epochs,
        )
        .map_err(|error| error.to_string())?;
    state
        .server
        .finish_key_index_contract_activation(&mut publication_guard, &tenant_id, &mut cutover)
        .await?;
    state.server.rebuild_reaction_dispatcher();
    activate_deploy_wasm_modules(state, input).await?;
    state
        .server
        .complete_spec_publication_retry(&mut publication_guard, &tenant_id)
}
