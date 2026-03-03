//! Post-transition hooks for system entity side effects.
//!
//! When a `temper-system` entity transitions and produces an
//! [`Effect::Custom`] effect, the hook dispatcher routes it to the
//! appropriate handler. This keeps `temper-server` generic — hooks are
//! registered at startup by `temper-platform`.
//!
//! Currently supported hooks:
//! - `DeploySpecs`: Triggered when a Tenant entity transitions to Active
//!   via the Deploy action. Runs the verify-and-deploy pipeline and
//!   registers the tenant's specs in the SpecRegistry.

use crate::deploy::pipeline::{DeployInput, DeployPipeline, EntitySpecSource};
use crate::state::PlatformState;

/// Dispatch a custom effect from a system entity transition.
///
/// Returns `Ok(())` if the hook ran successfully or the effect was
/// unrecognized (silently ignored). Returns `Err` if the hook failed.
pub fn dispatch_custom_effect(
    effect_name: &str,
    entity_type: &str,
    entity_id: &str,
    _params: &serde_json::Value,
    state: &PlatformState,
) -> Result<(), String> {
    match effect_name {
        "DeploySpecs" => handle_deploy_specs(entity_type, entity_id, state),
        _ => {
            tracing::debug!(
                effect = effect_name,
                entity_type = entity_type,
                entity_id = entity_id,
                "Unknown custom effect — ignored"
            );
            Ok(())
        }
    }
}

/// Handle the DeploySpecs effect: verify and register tenant specs.
///
/// Reads specs from the [`SpecStore`], builds a [`DeployInput`], and runs
/// the verify-and-deploy pipeline. On success, removes specs from the store.
fn handle_deploy_specs(
    _entity_type: &str,
    entity_id: &str,
    state: &PlatformState,
) -> Result<(), String> {
    tracing::info!(
        tenant = entity_id,
        "DeploySpecs hook: running verify-and-deploy pipeline"
    );

    // Read specs from the store using entity_id as tenant key.
    let tenant_specs = {
        let store = state.spec_store.read().unwrap(); // ci-ok: infallible lock
        store.get(entity_id).cloned()
    };

    let Some(specs) = tenant_specs else {
        tracing::warn!(
            tenant = entity_id,
            "DeploySpecs hook: no specs found in store for tenant"
        );
        return Err(format!(
            "no specs found in spec store for tenant '{entity_id}'"
        ));
    };

    // Build DeployInput from stored specs.
    let entities: Vec<EntitySpecSource> = specs
        .ioa_sources
        .iter()
        .map(|(entity_type, ioa_source)| EntitySpecSource {
            entity_type: entity_type.clone(),
            ioa_source: ioa_source.clone(),
        })
        .collect();

    let input = DeployInput {
        tenant_name: entity_id.to_string(),
        csdl_xml: specs.csdl_xml.clone(),
        entities,
        wasm_modules: specs.wasm_modules.clone(),
    };

    // Run the verify-and-deploy pipeline.
    let result = DeployPipeline::verify_and_deploy(state, &input);

    if result.success {
        tracing::info!(tenant = entity_id, "DeploySpecs hook: pipeline succeeded");
        // Remove specs from store on success.
        let mut store = state.spec_store.write().unwrap(); // ci-ok: infallible lock
        store.remove(entity_id);
        Ok(())
    } else {
        let failures: Vec<String> = result
            .entity_results
            .iter()
            .filter(|r| !r.verified)
            .map(|r| format!("{}: verification failed", r.entity_name))
            .collect();
        let summary = failures.join("; ");
        tracing::error!(
            tenant = entity_id,
            summary = %summary,
            "DeploySpecs hook: pipeline failed"
        );
        Err(format!(
            "deploy pipeline failed for tenant '{entity_id}': {summary}"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dispatch_unknown_effect_is_ok() {
        let state = PlatformState::new(None);
        let result = dispatch_custom_effect(
            "UnknownEffect",
            "Tenant",
            "t-1",
            &serde_json::json!({}),
            &state,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn test_dispatch_deploy_specs_no_store_entry() {
        let state = PlatformState::new(None);
        let result = dispatch_custom_effect(
            "DeploySpecs",
            "Tenant",
            "t-1",
            &serde_json::json!({}),
            &state,
        );
        // No specs in store → error
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no specs found"));
    }
}
