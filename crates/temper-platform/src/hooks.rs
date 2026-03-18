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
//! - `GenerateCedarPolicy`: Triggered when a GovernanceDecision entity
//!   transitions to Approved. Generates a Cedar permit policy from the
//!   entity's fields and reloads the authz engine.

use crate::deploy::{DeployInput, DeployPipeline, EntitySpecSource};
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
        "GenerateCedarPolicy" => {
            handle_generate_cedar_policy(entity_type, entity_id, _params, state)
        }
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

/// Handle the GenerateCedarPolicy effect: generate and load Cedar policy.
///
/// Triggered when a GovernanceDecision entity transitions to Approved.
/// Reads the entity's fields from the action params, generates a Cedar
/// permit statement based on the scope, validates the combined policy set,
/// and reloads the authz engine.
fn handle_generate_cedar_policy(
    _entity_type: &str,
    entity_id: &str,
    params: &serde_json::Value,
    state: &PlatformState,
) -> Result<(), String> {
    tracing::info!(
        entity_id = entity_id,
        "GenerateCedarPolicy hook: generating Cedar policy from GovernanceDecision"
    );

    let agent_id = params
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let action_name = params
        .get("action_name")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let resource_type = params
        .get("resource_type")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let resource_id = params
        .get("resource_id")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let scope = params
        .get("scope")
        .and_then(|v| v.as_str())
        .unwrap_or("narrow");
    let tenant = params.get("tenant").and_then(|v| v.as_str()).unwrap_or("");

    if agent_id.is_empty() || action_name.is_empty() || resource_type.is_empty() {
        return Err(format!(
            "GenerateCedarPolicy: missing required fields for entity '{entity_id}'"
        ));
    }

    // Parse scope_matrix from params, or build a default matrix based on the legacy scope string.
    let matrix: temper_authz::PolicyScopeMatrix =
        if let Some(matrix_val) = params.get("scope_matrix") {
            serde_json::from_value(matrix_val.clone()).map_err(|e| {
                format!("GenerateCedarPolicy: invalid scope_matrix for entity '{entity_id}': {e}")
            })?
        } else {
            match scope {
                "narrow" => temper_authz::PolicyScopeMatrix {
                    principal: temper_authz::PrincipalScope::ThisAgent,
                    action: temper_authz::ActionScope::ThisAction,
                    resource: temper_authz::ResourceScope::ThisResource,
                    duration: temper_authz::DurationScope::Always,
                    agent_type_value: None,
                    role_value: None,
                    session_id: None,
                },
                "broad" => temper_authz::PolicyScopeMatrix {
                    principal: temper_authz::PrincipalScope::ThisAgent,
                    action: temper_authz::ActionScope::AllActionsOnType,
                    resource: temper_authz::ResourceScope::AnyOfType,
                    duration: temper_authz::DurationScope::Always,
                    agent_type_value: None,
                    role_value: None,
                    session_id: None,
                },
                _ => temper_authz::PolicyScopeMatrix::default_for(None),
            }
        };
    temper_authz::validate_policy_scope_matrix(&matrix).map_err(|e| {
        format!("GenerateCedarPolicy: invalid scope_matrix for entity '{entity_id}': {e}")
    })?;
    let principal_kind = params
        .get("principal_kind")
        .and_then(|v| v.as_str())
        .unwrap_or("Agent");
    let generated_policy = temper_authz::generate_cedar_from_matrix(
        agent_id,
        principal_kind,
        action_name,
        resource_type,
        resource_id,
        &matrix,
    );

    tracing::info!(
        entity_id = entity_id,
        tenant = tenant,
        scope = scope,
        "GenerateCedarPolicy hook: generated policy, validating and loading"
    );

    // Validate and reload the per-tenant policy set.
    {
        let Ok(mut policies) = state.server.tenant_policies.write() else {
            return Err("tenant_policies lock poisoned".to_string());
        };
        let entry = policies.entry(tenant.to_string()).or_default();
        if !entry.is_empty() {
            entry.push('\n');
        }
        entry.push_str(&generated_policy);

        let tenant_text = entry.clone();
        if let Err(e) = state
            .server
            .authz
            .reload_tenant_policies(tenant, &tenant_text)
        {
            tracing::error!(error = %e, "GenerateCedarPolicy: failed to reload policies");
            return Err(format!("Failed to reload policies: {e}"));
        }
    }

    tracing::info!(
        entity_id = entity_id,
        "GenerateCedarPolicy hook: policy loaded successfully"
    );
    Ok(())
}

/// Generate a Cedar permit statement for the given scope.
///
/// Legacy helper retained for tests; production code uses matrix-based
/// `temper_authz::generate_cedar_from_matrix` instead.
#[cfg(test)]
fn generate_cedar_permit(
    agent_id: &str,
    action_name: &str,
    resource_type: &str,
    resource_id: &str,
    scope: &str,
) -> String {
    match scope {
        "narrow" => {
            format!(
                "permit(\n  principal == Agent::\"{agent_id}\",\n  action == Action::\"{action_name}\",\n  resource == {resource_type}::\"{resource_id}\"\n);"
            )
        }
        "medium" => {
            format!(
                "permit(\n  principal == Agent::\"{agent_id}\",\n  action == Action::\"{action_name}\",\n  resource is {resource_type}\n);"
            )
        }
        "broad" => {
            format!(
                "permit(\n  principal == Agent::\"{agent_id}\",\n  action,\n  resource is {resource_type}\n);"
            )
        }
        _ => {
            // Default to narrow scope for safety.
            format!(
                "permit(\n  principal == Agent::\"{agent_id}\",\n  action == Action::\"{action_name}\",\n  resource == {resource_type}::\"{resource_id}\"\n);"
            )
        }
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
    fn test_generate_cedar_permit_narrow() {
        let policy =
            generate_cedar_permit("agent-1", "submitOrder", "Order", "order-123", "narrow");
        assert!(policy.contains("Agent::\"agent-1\""));
        assert!(policy.contains("Action::\"submitOrder\""));
        assert!(policy.contains("Order::\"order-123\""));
    }

    #[test]
    fn test_generate_cedar_permit_medium() {
        let policy =
            generate_cedar_permit("agent-1", "submitOrder", "Order", "order-123", "medium");
        assert!(policy.contains("Agent::\"agent-1\""));
        assert!(policy.contains("Action::\"submitOrder\""));
        assert!(policy.contains("resource is Order"));
        assert!(!policy.contains("order-123"));
    }

    #[test]
    fn test_generate_cedar_permit_broad() {
        let policy = generate_cedar_permit("agent-1", "submitOrder", "Order", "order-123", "broad");
        assert!(policy.contains("Agent::\"agent-1\""));
        assert!(policy.contains("resource is Order"));
        assert!(!policy.contains("submitOrder"));
    }

    #[test]
    fn test_dispatch_generate_cedar_policy_missing_fields() {
        let state = PlatformState::new(None);
        let result = dispatch_custom_effect(
            "GenerateCedarPolicy",
            "GovernanceDecision",
            "gd-1",
            &serde_json::json!({}),
            &state,
        );
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing required fields"));
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
