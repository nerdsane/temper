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

use std::sync::{Arc, RwLock};

use temper_server::ServerState;
use temper_server::state::custom_effects::CustomEffectHandler;

use crate::deploy::{DeployInput, DeployPipeline, EntitySpecSource};
use crate::state::PlatformState;

mod generate_cedar;
mod governance_callback;

/// Dispatch a custom effect from a system entity transition.
///
/// Returns `Ok(())` if the hook ran successfully or the effect was
/// unrecognized (silently ignored). Returns `Err` if the hook failed.
pub async fn dispatch_custom_effect(
    effect_name: &str,
    entity_type: &str,
    entity_id: &str,
    _params: &serde_json::Value,
    state: &PlatformState,
) -> Result<(), String> {
    match effect_name {
        "DeploySpecs" => handle_deploy_specs(entity_type, entity_id, state).await,
        "GenerateCedarPolicy" => {
            handle_generate_cedar_policy(entity_type, entity_id, _params, state).await
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
async fn handle_deploy_specs(
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
    let result = DeployPipeline::verify_and_deploy(state, &input).await;

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
async fn handle_generate_cedar_policy(
    _entity_type: &str,
    entity_id: &str,
    params: &serde_json::Value,
    state: &PlatformState,
) -> Result<(), String> {
    generate_cedar::handle_generate_cedar_from_fields(entity_id, params, &state.server).await
}

// ---------------------------------------------------------------------------
// Platform custom effect handler (registered on ServerState)
// ---------------------------------------------------------------------------

/// Platform-level custom effect handler.
///
/// Registered on `ServerState` during `PlatformState` construction to
/// route custom effects from system entities to platform hooks.
pub struct PlatformEffectHandler {
    /// Spec store for DeploySpecs hook (future use).
    pub spec_store: Arc<RwLock<crate::spec_store::SpecStore>>,
}

#[async_trait::async_trait]
impl CustomEffectHandler for PlatformEffectHandler {
    async fn handle(
        &self,
        effect_name: &str,
        entity_type: &str,
        entity_id: &str,
        entity_fields: &serde_json::Value,
        server: &ServerState,
    ) -> Result<(), String> {
        match effect_name {
            "GenerateCedarPolicy" => {
                generate_cedar::handle_generate_cedar_from_fields(entity_id, entity_fields, server)
                    .await
            }
            "DispatchCallback" => {
                governance_callback::handle_dispatch_callback(entity_fields, server)
                    .await
            }
            _ => {
                tracing::debug!(
                    effect = effect_name,
                    entity_type,
                    entity_id,
                    "Unknown custom effect — ignored"
                );
                Ok(())
            }
        }
    }
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

    #[tokio::test]
    async fn test_dispatch_unknown_effect_is_ok() {
        let state = PlatformState::new(None);
        let result = dispatch_custom_effect(
            "UnknownEffect",
            "Tenant",
            "t-1",
            &serde_json::json!({}),
            &state,
        )
        .await;
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

    #[tokio::test]
    async fn test_dispatch_generate_cedar_policy_missing_fields() {
        let state = PlatformState::new(None);
        let result = dispatch_custom_effect(
            "GenerateCedarPolicy",
            "GovernanceDecision",
            "gd-1",
            &serde_json::json!({}),
            &state,
        )
        .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("missing required fields"));
    }

    #[tokio::test]
    async fn test_generate_cedar_policy_skips_api_owned_publication() {
        let state = PlatformState::new(None);
        let result = dispatch_custom_effect(
            "GenerateCedarPolicy",
            "GovernanceDecision",
            "gd-api-owned",
            &serde_json::json!({"policy_already_published": true}),
            &state,
        )
        .await;
        assert!(
            result.is_ok(),
            "API-owned policy must not be generated twice"
        );
    }

    #[tokio::test]
    async fn test_generate_cedar_policy_persists_one_stable_decision_entry() {
        let mut state = PlatformState::new(None);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "temper-generated-policy-{}.db",
            uuid::Uuid::new_v4()
        ));
        let store =
            temper_store_turso::TursoEventStore::new(&format!("file:{}", path.display()), None)
                .await
                .expect("create durable test store");
        state
            .server
            .set_storage_stack(temper_server::storage::StorageStack::from_turso(
                store.clone(),
            ));
        let fields = serde_json::json!({
            "agent_id": "agent-1",
            "action_name": "read",
            "resource_type": "Document",
            "resource_id": "doc-1",
            "scope": "narrow",
            "tenant": "tenant-policy-test",
            "decided_by": "reviewer-1",
        });

        for _ in 0..2 {
            dispatch_custom_effect(
                "GenerateCedarPolicy",
                "GovernanceDecision",
                "gd-stable",
                &fields,
                &state,
            )
            .await
            .expect("publish generated policy generation");
        }

        let policies = store
            .load_policies_for_tenant("tenant-policy-test")
            .await
            .expect("read durable policies");
        assert_eq!(policies.len(), 1);
        assert_eq!(policies[0].policy_id, "decision:gd-stable");
    }

    #[tokio::test]
    async fn test_dispatch_deploy_specs_no_store_entry() {
        let state = PlatformState::new(None);
        let result = dispatch_custom_effect(
            "DeploySpecs",
            "Tenant",
            "t-1",
            &serde_json::json!({}),
            &state,
        )
        .await;
        // No specs in store → error
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("no specs found"));
    }
}
