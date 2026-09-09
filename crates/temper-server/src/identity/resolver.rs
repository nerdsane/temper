//! Credential-to-identity resolution.
//!
//! Hashes bearer tokens, looks up `AgentCredential` entities, verifies the
//! linked `AgentType` is active, and returns a `ResolvedIdentity` that the
//! security context uses as the authoritative agent identity.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use crate::entity_actor::{EntityState, recover_authoritative_entity_state_from_store};
use crate::state::ServerState;

/// Maximum opaque credential size accepted by the identity boundary.
pub const MAX_CREDENTIAL_BYTES: usize = 8 * 1024;

/// A platform-resolved agent identity.
///
/// All fields are derived from the credential registry — never from
/// self-declared headers or client-reported values.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedIdentity {
    /// Platform-assigned unique agent instance ID (UUIDv7).
    pub agent_instance_id: String,
    /// The AgentType entity ID this credential is linked to.
    pub agent_type_id: String,
    /// The AgentType's human-readable name (e.g., "claude-code").
    pub agent_type_name: String,
    /// Whether this identity was verified through the credential registry.
    pub verified: bool,
}

/// Resolves bearer tokens to platform-assigned agent identities.
///
/// Each protected request resolves both the credential and its linked agent
/// type from authoritative state. Persistent deployments replay the complete
/// durable journal with strict validation; in-memory deployments read their
/// sole local actor state. Successful identities are deliberately not cached,
/// so revocation and type deprecation take effect on the next request even
/// when another server replica performed the mutation.
#[derive(Clone, Copy, Debug, Default)]
pub struct IdentityResolver;

impl IdentityResolver {
    /// Create a new identity resolver.
    pub fn new() -> Self {
        Self
    }

    /// Resolve a bearer token to a verified agent identity.
    ///
    /// 1. Hash the token (SHA-256)
    /// 2. Read `AgentCredential` by using key_hash as entity ID
    /// 3. Verify credential is `Active` and unexpired
    /// 4. Read the linked `AgentType`
    /// 5. Verify AgentType is `Active`
    /// 6. Return the verified identity without retaining positive authority
    pub async fn resolve(
        &self,
        state: &ServerState,
        tenant: &TenantId,
        bearer_token: &str,
    ) -> Option<ResolvedIdentity> {
        if bearer_token.is_empty() || bearer_token.len() > MAX_CREDENTIAL_BYTES {
            return None;
        }
        let key_hash = hash_token(bearer_token);

        // Look up AgentCredential entity. We use the key_hash as entity ID
        // for O(1) lookup — the Issue action must use the key_hash as the
        // entity ID when creating credentials.
        let credential =
            authoritative_entity_state(state, tenant, "AgentCredential", &key_hash).await?;

        // Verify credential is Active.
        if credential.status != "Active" {
            return None;
        }

        let fields = &credential.fields;
        let credential_expires_at = match parse_credential_expiry(fields) {
            Ok(expires_at) => expires_at,
            Err(error) => {
                tracing::warn!(tenant = %tenant, %error, "credential has invalid expiration metadata");
                return None;
            }
        };
        if credential_expires_at.is_some_and(|expires_at| sim_now() >= expires_at) {
            return None;
        }
        let agent_type_id = fields.get("agent_type_id")?.as_str()?;
        let agent_instance_id = fields.get("agent_instance_id")?.as_str()?;
        let stored_key_hash = fields.get("key_hash")?.as_str()?;

        if agent_type_id.is_empty() || agent_instance_id.is_empty() || stored_key_hash != key_hash {
            return None;
        }

        // Look up linked AgentType entity.
        let agent_type =
            authoritative_entity_state(state, tenant, "AgentType", agent_type_id).await?;

        // Verify AgentType is Active.
        if agent_type.status != "Active" {
            return None;
        }

        let agent_type_name = agent_type
            .fields
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|name| !name.is_empty())?
            .to_string();

        // The credential and linked type are separate actors/streams. Re-read
        // the credential after observing the type and require an identical
        // authority-bearing snapshot. This establishes a point during the type
        // read at which both were active; without it, a revocation or link
        // change between the two reads could assemble a mixed-time identity
        // that never existed.
        let credential_recheck =
            authoritative_entity_state(state, tenant, "AgentCredential", &key_hash).await?;
        if !same_credential_authority(&credential, &credential_recheck) {
            tracing::warn!(
                tenant = %tenant,
                credential = %key_hash,
                "credential authority changed during identity resolution"
            );
            return None;
        }

        let identity = ResolvedIdentity {
            agent_instance_id: agent_instance_id.to_string(),
            agent_type_id: agent_type_id.to_string(),
            agent_type_name,
            verified: true,
        };

        // Re-check after the linked AgentType lookup so a short-lived
        // credential cannot cross its expiry while resolution is in flight.
        if credential_expires_at.is_some_and(|expires_at| sim_now() >= expires_at) {
            return None;
        }

        Some(identity)
    }
}

fn same_credential_authority(first: &EntityState, second: &EntityState) -> bool {
    first.entity_type == second.entity_type
        && first.entity_id == second.entity_id
        && first.sequence_nr == second.sequence_nr
        && first.status == second.status
        && first.fields == second.fields
}

async fn authoritative_entity_state(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
) -> Option<EntityState> {
    let Some((store, backend)) = state.event_journal() else {
        return state
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
            .ok()
            .map(|response| response.state);
    };

    let table = state.registry.read().ok()?.get_table(tenant, entity_type)?;
    let initial_fields = serde_json::json!({});
    match recover_authoritative_entity_state_from_store(
        tenant.as_str(),
        entity_type,
        entity_id,
        table.as_ref(),
        &store,
        backend,
        &initial_fields,
        None,
    )
    .await
    {
        Ok(entity) if entity.total_event_count > 0 => Some(entity),
        Ok(_) => None,
        Err(error) => {
            tracing::warn!(
                tenant = %tenant,
                entity_type,
                entity_id,
                %error,
                "authoritative identity state replay failed closed"
            );
            None
        }
    }
}

fn parse_credential_expiry(fields: &serde_json::Value) -> Result<Option<DateTime<Utc>>, String> {
    let Some(value) = fields.get("expires_at") else {
        return Ok(None);
    };
    let value = value
        .as_str()
        .ok_or_else(|| "expires_at must be an RFC3339 string".to_string())?
        .trim();
    if value.is_empty() {
        return Ok(None);
    }
    DateTime::parse_from_rfc3339(value)
        .map(|expires_at| Some(expires_at.with_timezone(&Utc)))
        .map_err(|error| format!("expires_at is not valid RFC3339: {error}"))
}

/// Hash a bearer token with SHA-256 for credential lookup.
pub fn hash_token(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let hash_bytes = hasher.finalize();
    hash_bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_token_deterministic() {
        let h1 = hash_token("test-token-123");
        let h2 = hash_token("test-token-123");
        assert_eq!(h1, h2);
        assert_eq!(h1.len(), 64); // SHA-256 = 64 hex chars
    }

    #[test]
    fn test_hash_token_different_inputs() {
        let h1 = hash_token("token-a");
        let h2 = hash_token("token-b");
        assert_ne!(h1, h2);
    }

    #[test]
    fn credential_expiry_is_optional_but_malformed_values_fail_closed() {
        assert_eq!(
            parse_credential_expiry(&serde_json::json!({"expires_at": ""})),
            Ok(None)
        );
        assert_eq!(parse_credential_expiry(&serde_json::json!({})), Ok(None));
        assert!(parse_credential_expiry(&serde_json::json!({"expires_at": "tomorrow"})).is_err());
        assert!(parse_credential_expiry(&serde_json::json!({"expires_at": 42})).is_err());
        assert_eq!(
            parse_credential_expiry(&serde_json::json!({
                "expires_at": "2030-01-02T03:04:05+02:00"
            }))
            .expect("valid RFC3339 expiry")
            .expect("expiry present")
            .to_rfc3339(),
            "2030-01-02T01:04:05+00:00"
        );
    }

    #[test]
    fn credential_stability_check_binds_sequence_status_and_fields() {
        let state = |sequence_nr, status: &str, fields: serde_json::Value| EntityState {
            entity_type: "AgentCredential".to_string(),
            entity_id: "hash".to_string(),
            status: status.to_string(),
            item_count: 0,
            counters: Default::default(),
            booleans: Default::default(),
            lists: Default::default(),
            fields,
            events: Default::default(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr,
            processed_idempotency_keys: Default::default(),
        };
        let first = state(3, "Active", serde_json::json!({"agent_type_id": "type-a"}));

        assert!(same_credential_authority(&first, &first.clone()));
        assert!(!same_credential_authority(
            &first,
            &state(4, "Active", first.fields.clone())
        ));
        assert!(!same_credential_authority(
            &first,
            &state(3, "Revoked", first.fields.clone())
        ));
        assert!(!same_credential_authority(
            &first,
            &state(3, "Active", serde_json::json!({"agent_type_id": "type-b"}))
        ));
    }
}
