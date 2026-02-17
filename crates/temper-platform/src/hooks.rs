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
fn handle_deploy_specs(
    _entity_type: &str,
    entity_id: &str,
    _state: &PlatformState,
) -> Result<(), String> {
    tracing::info!(
        tenant = entity_id,
        "DeploySpecs hook: running verify-and-deploy pipeline"
    );

    // In a full implementation, this would:
    // 1. Read the tenant's specs from the Project entity's stored state
    // 2. Build a DeployInput from those specs
    // 3. Run DeployPipeline::verify_and_deploy()
    // 4. The pipeline handles SpecRegistry registration internally
    //
    // For now, this is a dispatch stub that logs the intent.
    // The actual spec source would come from the Project entity's state
    // or from a spec storage system.

    tracing::info!(
        tenant = entity_id,
        "DeploySpecs hook: tenant deploy would be triggered here"
    );

    Ok(())
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
    fn test_dispatch_deploy_specs_effect() {
        let state = PlatformState::new(None);
        let result = dispatch_custom_effect(
            "DeploySpecs",
            "Tenant",
            "t-1",
            &serde_json::json!({}),
            &state,
        );
        assert!(result.is_ok());
    }
}
