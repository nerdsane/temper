//! Target-minted, server-authenticated governance callback capabilities.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use crate::state::ServerState;

const CAPABILITY_VERSION: u8 = 1;
const CAPABILITY_LIFETIME_DAYS: i64 = 30;
const MAX_CALLBACK_COMPONENT_BYTES: usize = 1024;
const MAX_ENCODED_CAPABILITY_BYTES: usize = 16 * 1024;

/// Exact authority a waiting target grants to one GovernanceDecision callback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GovernanceCallbackCapability {
    /// Capability format version.
    pub version: u8,
    /// GovernanceDecision actor authorized to consume this capability.
    pub source_governance_decision_id: String,
    /// Tenant of the target that minted the capability.
    pub target_tenant: String,
    /// Governed entity type of the target that minted the capability.
    pub target_entity_type: String,
    /// Exact target entity id.
    pub target_entity_id: String,
    /// Only action permitted for approval delivery.
    pub approve_action: String,
    /// Only action permitted for denial delivery.
    pub deny_action: String,
    /// Deterministic UTC expiry timestamp.
    pub expires_at: String,
    /// Stable content-derived delivery identity.
    pub delivery_id: String,
}

fn nonempty(name: &str, value: &str) -> Result<(), String> {
    if value.trim().is_empty() {
        return Err(format!("callback capability {name} must not be empty"));
    }
    if value.len() > MAX_CALLBACK_COMPONENT_BYTES {
        return Err(format!(
            "callback capability {name} exceeds {MAX_CALLBACK_COMPONENT_BYTES} bytes"
        ));
    }
    Ok(())
}

fn write_component(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_be_bytes());
    hasher.update(value.as_bytes());
}

fn delivery_id(
    source_governance_decision_id: &str,
    target_tenant: &str,
    target_entity_type: &str,
    target_entity_id: &str,
    approve_action: &str,
    deny_action: &str,
) -> String {
    let mut hasher = Sha256::new();
    for component in [
        source_governance_decision_id,
        target_tenant,
        target_entity_type,
        target_entity_id,
        approve_action,
        deny_action,
    ] {
        write_component(&mut hasher, component);
    }
    format!("callback:{:x}", hasher.finalize())
}

impl ServerState {
    /// Mint one capability from the exact target actor currently invoking WASM.
    pub fn mint_governance_callback_capability(
        &self,
        source_governance_decision_id: &str,
        target_tenant: &str,
        target_entity_type: &str,
        target_entity_id: &str,
        approve_action: &str,
        deny_action: &str,
    ) -> Result<String, String> {
        for (name, value) in [
            ("source decision id", source_governance_decision_id),
            ("target tenant", target_tenant),
            ("target entity type", target_entity_type),
            ("target entity id", target_entity_id),
            ("approve action", approve_action),
            ("deny action", deny_action),
        ] {
            nonempty(name, value)?;
        }
        let vault = self
            .secrets_vault
            .as_ref()
            .ok_or_else(|| "callback capability signer is not configured".to_string())?;
        let expires_at =
            (sim_now() + chrono::Duration::days(CAPABILITY_LIFETIME_DAYS)).to_rfc3339();
        let capability = GovernanceCallbackCapability {
            version: CAPABILITY_VERSION,
            source_governance_decision_id: source_governance_decision_id.to_string(),
            target_tenant: target_tenant.to_string(),
            target_entity_type: target_entity_type.to_string(),
            target_entity_id: target_entity_id.to_string(),
            approve_action: approve_action.to_string(),
            deny_action: deny_action.to_string(),
            expires_at,
            delivery_id: delivery_id(
                source_governance_decision_id,
                target_tenant,
                target_entity_type,
                target_entity_id,
                approve_action,
                deny_action,
            ),
        };
        let payload = serde_json::to_vec(&capability)
            .map_err(|error| format!("failed to encode callback capability: {error}"))?;
        let signature = vault.sign_callback_capability(&payload).ok_or_else(|| {
            "callback capability signing requires a stable TEMPER_VAULT_KEY".to_string()
        })?;
        Ok(format!(
            "{}.{}",
            URL_SAFE_NO_PAD.encode(payload),
            URL_SAFE_NO_PAD.encode(signature)
        ))
    }

    /// Verify authenticity, version, expiry, and structural integrity.
    pub fn verify_governance_callback_capability(
        &self,
        encoded: &str,
    ) -> Result<GovernanceCallbackCapability, String> {
        if encoded.len() > MAX_ENCODED_CAPABILITY_BYTES {
            return Err(format!(
                "callback capability exceeds {MAX_ENCODED_CAPABILITY_BYTES} encoded bytes"
            ));
        }
        let (payload, signature) = encoded
            .split_once('.')
            .ok_or_else(|| "callback capability is not a signed token".to_string())?;
        let payload = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| "callback capability payload is not valid base64url".to_string())?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| "callback capability signature is not valid base64url".to_string())?;
        let vault = self
            .secrets_vault
            .as_ref()
            .ok_or_else(|| "callback capability verifier is not configured".to_string())?;
        if !vault.verify_callback_capability(&payload, &signature) {
            return Err("callback capability signature is invalid".to_string());
        }
        let capability: GovernanceCallbackCapability = serde_json::from_slice(&payload)
            .map_err(|error| format!("invalid callback capability payload: {error}"))?;
        if capability.version != CAPABILITY_VERSION {
            return Err(format!(
                "unsupported callback capability version {}",
                capability.version
            ));
        }
        for (name, value) in [
            (
                "source decision id",
                capability.source_governance_decision_id.as_str(),
            ),
            ("target tenant", capability.target_tenant.as_str()),
            ("target entity type", capability.target_entity_type.as_str()),
            ("target entity id", capability.target_entity_id.as_str()),
            ("approve action", capability.approve_action.as_str()),
            ("deny action", capability.deny_action.as_str()),
        ] {
            nonempty(name, value)?;
        }
        let expires_at = chrono::DateTime::parse_from_rfc3339(&capability.expires_at)
            .map_err(|_| "callback capability expiry is invalid".to_string())?
            .with_timezone(&chrono::Utc);
        if sim_now() >= expires_at {
            return Err("callback capability has expired".to_string());
        }
        let expected_delivery_id = delivery_id(
            &capability.source_governance_decision_id,
            &capability.target_tenant,
            &capability.target_entity_type,
            &capability.target_entity_id,
            &capability.approve_action,
            &capability.deny_action,
        );
        if capability.delivery_id != expected_delivery_id {
            return Err("callback capability delivery id is invalid".to_string());
        }
        Ok(capability)
    }

    /// Verify that registration/delivery fields are exactly capability-bound.
    pub fn validate_governance_callback_binding(
        &self,
        source_governance_decision_id: &str,
        fields: &serde_json::Value,
        encoded: &str,
    ) -> Result<GovernanceCallbackCapability, String> {
        let capability = self.verify_governance_callback_capability(encoded)?;
        let required = |name: &str| {
            fields
                .get(name)
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("callback registration is missing {name:?}"))
        };
        let target_tenant = required("callback_tenant")?;
        let target_set = required("callback_entity_set")?;
        let target_id = required("callback_entity_id")?;
        let approve_action = required("callback_on_approve")?;
        let deny_action = required("callback_on_deny")?;
        let tenant = TenantId::new(target_tenant);
        let target_type = {
            let registry = match self.registry.read() {
                Ok(registry) => registry,
                Err(poisoned) => poisoned.into_inner(),
            };
            registry
                .resolve_entity_type(&tenant, target_set)
                .or_else(|| {
                    registry
                        .get_spec(&tenant, target_set)
                        .is_some()
                        .then(|| target_set.to_string())
                })
        }
        .ok_or_else(|| format!("callback target type {target_set:?} is not governed"))?;
        let expected = (
            source_governance_decision_id,
            target_tenant,
            target_type.as_str(),
            target_id,
            approve_action,
            deny_action,
        );
        let actual = (
            capability.source_governance_decision_id.as_str(),
            capability.target_tenant.as_str(),
            capability.target_entity_type.as_str(),
            capability.target_entity_id.as_str(),
            capability.approve_action.as_str(),
            capability.deny_action.as_str(),
        );
        if actual != expected {
            return Err(
                "callback registration fields do not match target-minted capability".to_string(),
            );
        }
        Ok(capability)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::CsdlDocument;

    fn state() -> ServerState {
        ServerState::new(
            ActorSystem::new("callback-capability-test"),
            CsdlDocument {
                version: "4.0".to_string(),
                schemas: Vec::new(),
            },
            String::new(),
        )
        .with_secrets_vault(crate::secrets::vault::SecretsVault::new(&[7; 32]))
    }

    fn state_with_session_mapping() -> ServerState {
        let xml = r#"<?xml version="1.0"?>
            <edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
              <edmx:DataServices>
                <Schema Namespace="Test" xmlns="http://docs.oasis-open.org/odata/ns/edm">
                  <EntityType Name="Session"><Key><PropertyRef Name="Id"/></Key><Property Name="Id" Type="Edm.String" Nullable="false"/></EntityType>
                  <EntityContainer Name="Container"><EntitySet Name="Sessions" EntityType="Test.Session"/></EntityContainer>
                </Schema>
              </edmx:DataServices>
            </edmx:Edmx>"#;
        let csdl = temper_spec::csdl::parse_csdl(xml).expect("parse callback fixture CSDL");
        let ioa = r#"
                    [automaton]
                    name = "Session"
                    initial = "Waiting"
                    states = ["Waiting"]
                "#;
        let state = ServerState::new(
            ActorSystem::new("callback-capability-binding-test"),
            csdl.clone(),
            xml.to_string(),
        )
        .with_secrets_vault(crate::secrets::vault::SecretsVault::new(&[7; 32]));
        state
            .registry
            .write()
            .expect("registry lock")
            .register_tenant("tenant-a", csdl, xml.to_string(), &[("Session", ioa)]);
        state
    }

    #[test]
    fn capability_round_trip_and_tamper_rejection() {
        let state = state();
        let encoded = state
            .mint_governance_callback_capability(
                "gd-1",
                "tenant-a",
                "Session",
                "session-1",
                "Resume",
                "Fail",
            )
            .expect("mint capability");
        let capability = state
            .verify_governance_callback_capability(&encoded)
            .expect("verify capability");
        assert_eq!(capability.source_governance_decision_id, "gd-1");
        assert_eq!(capability.target_entity_type, "Session");

        let mut tampered = encoded.into_bytes();
        let index = tampered.len() / 2;
        tampered[index] = if tampered[index] == b'A' { b'B' } else { b'A' };
        assert!(
            state
                .verify_governance_callback_capability(
                    std::str::from_utf8(&tampered).expect("ASCII token")
                )
                .is_err()
        );
    }

    #[test]
    fn binding_rejects_every_free_form_target_change() {
        let state = state_with_session_mapping();
        let encoded = state
            .mint_governance_callback_capability(
                "gd-1",
                "tenant-a",
                "Session",
                "session-1",
                "Resume",
                "Fail",
            )
            .expect("mint capability");
        let fields = serde_json::json!({
            "callback_tenant": "tenant-a",
            "callback_entity_set": "Sessions",
            "callback_entity_id": "session-1",
            "callback_on_approve": "Resume",
            "callback_on_deny": "Fail",
        });
        state
            .validate_governance_callback_binding("gd-1", &fields, &encoded)
            .expect("exact binding");

        for (field, tampered) in [
            ("callback_tenant", "tenant-b"),
            ("callback_entity_set", "OtherSessions"),
            ("callback_entity_id", "session-2"),
            ("callback_on_approve", "Delete"),
            ("callback_on_deny", "Escalate"),
        ] {
            let mut changed = fields.clone();
            changed[field] = serde_json::Value::String(tampered.to_string());
            assert!(
                state
                    .validate_governance_callback_binding("gd-1", &changed, &encoded)
                    .is_err(),
                "tampering {field} must fail"
            );
        }
        assert!(
            state
                .validate_governance_callback_binding("gd-2", &fields, &encoded)
                .is_err()
        );
    }

    #[test]
    fn capability_expiry_is_enforced_by_simulated_time() {
        let (_guard, clock, _ids) = temper_runtime::scheduler::install_deterministic_context(17);
        let state = state();
        let encoded = state
            .mint_governance_callback_capability(
                "gd-expiry",
                "tenant-a",
                "Session",
                "session-1",
                "Resume",
                "Fail",
            )
            .expect("mint capability");
        state
            .verify_governance_callback_capability(&encoded)
            .expect("capability starts valid");
        clock.advance_by(25_920_001);
        let error = state
            .verify_governance_callback_capability(&encoded)
            .expect_err("capability must expire");
        assert!(error.contains("expired"));
    }

    #[test]
    fn capability_component_and_token_budgets_fail_before_decode_or_sign() {
        let state = state();
        let oversized = "x".repeat(MAX_CALLBACK_COMPONENT_BYTES + 1);
        let error = state
            .mint_governance_callback_capability(
                "gd-budget",
                "tenant-a",
                "Session",
                &oversized,
                "Resume",
                "Fail",
            )
            .expect_err("oversized component must fail");
        assert!(error.contains("exceeds"));
        let oversized_token = "A".repeat(MAX_ENCODED_CAPABILITY_BYTES + 1);
        let error = state
            .verify_governance_callback_capability(&oversized_token)
            .expect_err("oversized token must fail");
        assert!(error.contains("encoded bytes"));
    }

    #[test]
    fn ephemeral_vault_key_cannot_mint_cross_replica_capability() {
        let state = ServerState::new(
            ActorSystem::new("callback-capability-ephemeral-test"),
            CsdlDocument {
                version: "4.0".to_string(),
                schemas: Vec::new(),
            },
            String::new(),
        )
        .with_secrets_vault(crate::secrets::vault::SecretsVault::new_ephemeral(&[7; 32]));
        let error = state
            .mint_governance_callback_capability(
                "gd-ephemeral",
                "tenant-a",
                "Session",
                "session-1",
                "Resume",
                "Fail",
            )
            .expect_err("ephemeral signing key must fail closed");
        assert!(error.contains("stable TEMPER_VAULT_KEY"));
    }
}
