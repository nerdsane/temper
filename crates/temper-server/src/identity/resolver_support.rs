//! Standalone helpers for credential-to-identity resolution.
//!
//! Split out of `resolver.rs` to keep both files under the repository's
//! 500-line-per-file convention. These functions have no `IdentityResolver`
//! state of their own: token hashing/shape detection, the identity entity
//! read that distinguishes a failed read from a confirmed-absent record, and small field/expiry parsing used while assembling a
//! `ResolvedIdentity`. `pub(super)` scopes them to `crate::identity` and its
//! descendants — visible to `resolver.rs`, not exported further.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};

use crate::entity_actor::EntityState;
use crate::identity::error::IdentityError;
use crate::state::ServerState;
use temper_runtime::tenant::TenantId;

pub(super) fn same_credential_authority(first: &EntityState, second: &EntityState) -> bool {
    first.entity_type == second.entity_type
        && first.entity_id == second.entity_id
        && first.sequence_nr == second.sequence_nr
        && first.status == second.status
        && first.fields == second.fields
}

/// Read a required string field from stored entity fields.
///
/// A missing or non-string field is corrupt/incomplete record data, not a
/// dependency outage — it classifies as [`IdentityError::Invalid`].
pub(super) fn require_field<'a>(
    fields: &'a serde_json::Value,
    name: &str,
) -> Result<&'a str, IdentityError> {
    fields
        .get(name)
        .and_then(|value| value.as_str())
        .ok_or(IdentityError::Invalid)
}

/// An identity entity's current state, read like any other entity.
///
/// `Ok(None)` means the entity confirmedly does not exist here: it has no
/// events, it was deleted, or the tenant has no governing transition table
/// for this type at all. The last is a configuration state (identity
/// governance never installed for this tenant), not an outage, so it falls
/// through like any other absent credential. `Err` means a read the answer
/// depends on failed (a poisoned spec registry, a durable store read, or the
/// actor ask), and callers route it through [`classify_dependency_read`] so it
/// is never reported as an invalid credential.
///
/// The existence check comes first because reading spawns an actor on
/// demand, and the id here comes from a caller's token. An entity this server
/// has not loaded since it started (after a restart, or one written by another
/// server) is found in the durable store once and then served from its actor.
///
/// [`classify_dependency_read`]: crate::identity::error::classify_dependency_read
pub(super) async fn entity_state(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
) -> Result<Option<EntityState>, String> {
    let governed = {
        let registry = state
            .registry
            .read()
            .map_err(|_| "spec registry lock poisoned".to_string())?;
        registry.get_table(tenant, entity_type).is_some()
    };
    if !governed {
        return Ok(None);
    }
    if !state.entity_exists(tenant, entity_type, entity_id)
        && !state
            .load_entity_from_store(tenant, entity_type, entity_id)
            .await?
    {
        return Ok(None);
    }
    state
        .get_tenant_entity_state(tenant, entity_type, entity_id)
        .await
        .map(|response| Some(response.state))
        .map_err(|error| {
            tracing::warn!(
                tenant = %tenant,
                entity_type,
                entity_id,
                %error,
                "identity state read failed; identity dependency unavailable"
            );
            error
        })
}

pub(super) fn parse_credential_expiry(
    fields: &serde_json::Value,
) -> Result<Option<DateTime<Utc>>, String> {
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
            last_event: Default::default(),
            status_clock: Default::default(),
            spawn_reservations: Default::default(),
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

/// A JWS compact serialization is exactly three non-empty dot-separated parts.
/// Opaque credential tokens (e.g. `kc_...`) never match, so they take the
/// registry path.
pub(super) fn looks_like_jwt(token: &str) -> bool {
    let mut parts = token.split('.');
    matches!(
        (parts.next(), parts.next(), parts.next(), parts.next()),
        (Some(h), Some(p), Some(s), None) if !h.is_empty() && !p.is_empty() && !s.is_empty()
    )
}

#[cfg(test)]
mod jwt_shape_tests {
    use super::looks_like_jwt;

    #[test]
    fn distinguishes_jwt_from_opaque() {
        assert!(looks_like_jwt("eyJhbGciOiJFUzI1NiJ9.eyJpc3MiOiJ4In0.c2ln"));
        assert!(!looks_like_jwt("kc_3f2a9b8c7d6e5f4a"));
        assert!(!looks_like_jwt(""));
        assert!(!looks_like_jwt("a.b"));
        assert!(!looks_like_jwt("a.b.c.d"));
    }
}
