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
//!   transitions to Approved. Verifies the exact decision-bound policy was
//!   already persisted and activated by the approval boundary.

use std::sync::{Arc, RwLock};

use temper_server::ServerState;
use temper_server::state::custom_effects::CustomEffectHandler;

#[cfg(test)]
use crate::deploy::{DeployInput, DeployPipeline, EntitySpecSource};
#[cfg(test)]
use crate::state::PlatformState;

mod generate_cedar;
mod governance_callback;
mod governance_resolution;

/// Dispatch a custom effect from a system entity transition.
///
/// Returns `Ok(())` if the hook ran successfully or the effect was
/// unrecognized (silently ignored). Returns `Err` if the hook failed.
#[cfg(test)]
pub fn dispatch_custom_effect(
    effect_name: &str,
    entity_type: &str,
    entity_id: &str,
    _params: &serde_json::Value,
    state: &PlatformState,
) -> Result<(), String> {
    match effect_name {
        "DeploySpecs" => handle_deploy_specs(entity_type, entity_id, state),
        "GenerateCedarPolicy" => generate_cedar::handle_generate_cedar_from_fields(
            entity_type,
            entity_id,
            _params,
            &state.server,
        ),
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
#[cfg(test)]
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
            "GenerateCedarPolicy" => generate_cedar::handle_generate_cedar_from_fields(
                entity_type,
                entity_id,
                entity_fields,
                server,
            ),
            "DispatchCallback" => {
                governance_callback::handle_dispatch_callback(entity_id, entity_fields, server)
                    .await
            }
            "CompleteGovernanceApproval" => {
                generate_cedar::handle_generate_cedar_from_fields_durable(
                    entity_type,
                    entity_id,
                    entity_fields,
                    server,
                )
                .await?;
                governance_callback::handle_dispatch_callback(entity_id, entity_fields, server)
                    .await?;
                governance_resolution::handle_finalize_governance_resolution(
                    entity_type,
                    entity_id,
                    entity_fields,
                    server,
                )
                .await
            }
            "CompleteGovernanceDenial" => {
                governance_callback::handle_dispatch_callback(entity_id, entity_fields, server)
                    .await?;
                governance_resolution::handle_finalize_governance_resolution(
                    entity_type,
                    entity_id,
                    entity_fields,
                    server,
                )
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

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;
    use temper_authz::{
        ActionScope, DurationScope, PolicyScopeMatrix, PrincipalScope, ResourceScope,
        SecurityContext,
    };
    use temper_server::authz::DecisionPolicyReceipt;

    fn session_customer_fixture(
        state: &PlatformState,
        duplicate: bool,
    ) -> (serde_json::Value, String) {
        let matrix = PolicyScopeMatrix {
            principal: PrincipalScope::ThisAgent,
            action: ActionScope::ThisAction,
            resource: ResourceScope::ThisResource,
            duration: DurationScope::Session,
            agent_type_value: None,
            role_value: None,
            session_id: Some("session-allowed".to_string()),
        };
        let policy = temper_authz::generate_cedar_from_matrix(
            "customer-1",
            "Customer",
            "read",
            "Order",
            "order-1",
            &matrix,
        )
        .expect("fixture policy should generate");
        let mut named = vec![("decision:pd-1".to_string(), policy.clone())];
        if duplicate {
            named.push(("manual-copy".to_string(), policy.clone()));
        }
        state
            .server
            .authz
            .reload_tenant_policies_named("tenant-a", &named)
            .expect("fixture policies should load");
        let receipt = DecisionPolicyReceipt {
            pending_decision_id: "pd-1".to_string(),
            governance_decision_id: "gd-1".to_string(),
            principal_kind: "Customer".to_string(),
            scope_matrix: matrix,
        };
        (
            serde_json::json!({
                "agent_id": "customer-1",
                "action_name": "read",
                "resource_type": "Order",
                "resource_id": "order-1",
                "tenant": "tenant-a",
                "pending_decision_id": "pd-1",
                "scope": receipt.encode().expect("receipt should encode"),
                "generated_policy": policy,
            }),
            policy,
        )
    }

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
        assert!(result.unwrap_err().contains("missing required field"));
    }

    #[test]
    fn generate_cedar_rejects_unbound_legacy_scope() {
        let state = PlatformState::new(None);
        let result = dispatch_custom_effect(
            "GenerateCedarPolicy",
            "GovernanceDecision",
            "gd-1",
            &serde_json::json!({
                "agent_id": "agent-1",
                "action_name": "read",
                "resource_type": "Order",
                "resource_id": "order-1",
                "tenant": "tenant-a",
                "pending_decision_id": "pd-1",
                "generated_policy": "permit(principal, action, resource);",
                "scope": "typo"
            }),
            &state,
        );

        assert!(
            result
                .unwrap_err()
                .contains("invalid decision policy receipt")
        );
    }

    #[test]
    fn receipt_verification_is_idempotent_and_preserves_customer_session_scope() {
        let state = PlatformState::new(None);
        let (fields, policy) = session_customer_fixture(&state, false);

        for _ in 0..2 {
            dispatch_custom_effect(
                "GenerateCedarPolicy",
                "GovernanceDecision",
                "gd-1",
                &fields,
                &state,
            )
            .expect("an exact preinstalled receipt should verify idempotently");
        }
        let active = state
            .server
            .authz
            .get_tenant_policy_text("tenant-a")
            .expect("tenant policy should remain loaded");
        assert_eq!(active, policy);
        assert_eq!(
            fields
                .get("generated_policy")
                .and_then(|value| value.as_str()),
            Some(policy.as_str())
        );

        let allowed_customer = SecurityContext::from_headers(&[
            (
                "X-Temper-Principal-Id".to_string(),
                "customer-1".to_string(),
            ),
            (
                "X-Temper-Principal-Kind".to_string(),
                "customer".to_string(),
            ),
            (
                "X-Temper-Ctx-SessionId".to_string(),
                "session-allowed".to_string(),
            ),
        ]);
        let mut attrs = HashMap::new();
        attrs.insert("id".to_string(), serde_json::json!("order-1"));
        assert!(
            state
                .server
                .authz
                .authorize_for_tenant("tenant-a", &allowed_customer, "read", "Order", &attrs)
                .is_allowed()
        );

        let wrong_session = SecurityContext::from_headers(&[
            (
                "X-Temper-Principal-Id".to_string(),
                "customer-1".to_string(),
            ),
            (
                "X-Temper-Principal-Kind".to_string(),
                "customer".to_string(),
            ),
            (
                "X-Temper-Ctx-SessionId".to_string(),
                "session-other".to_string(),
            ),
        ]);
        assert!(
            !state
                .server
                .authz
                .authorize_for_tenant("tenant-a", &wrong_session, "read", "Order", &attrs)
                .is_allowed()
        );

        let wrong_principal_kind = SecurityContext::from_headers(&[
            (
                "X-Temper-Principal-Id".to_string(),
                "customer-1".to_string(),
            ),
            ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
            (
                "X-Temper-Ctx-SessionId".to_string(),
                "session-allowed".to_string(),
            ),
        ]);
        assert!(
            !state
                .server
                .authz
                .authorize_for_tenant("tenant-a", &wrong_principal_kind, "read", "Order", &attrs,)
                .is_allowed()
        );
    }

    #[test]
    fn receipt_verification_rejects_duplicate_active_policy() {
        let state = PlatformState::new(None);
        let (fields, _) = session_customer_fixture(&state, true);
        let error = dispatch_custom_effect(
            "GenerateCedarPolicy",
            "GovernanceDecision",
            "gd-1",
            &fields,
            &state,
        )
        .expect_err("duplicate active permits must fail closed");
        assert!(error.contains("active 2 times"));
    }

    #[test]
    fn receipt_verification_rejects_policy_not_bound_to_matrix() {
        let state = PlatformState::new(None);
        let (mut fields, _) = session_customer_fixture(&state, false);
        fields["generated_policy"] =
            serde_json::Value::String("permit(principal, action, resource);".to_string());
        let error = dispatch_custom_effect(
            "GenerateCedarPolicy",
            "GovernanceDecision",
            "gd-1",
            &fields,
            &state,
        )
        .expect_err("policy text must exactly reproduce its receipt");
        assert!(error.contains("does not match the bound approval receipt"));
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
