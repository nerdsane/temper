//! Credential-to-identity resolution.
//!
//! Hashes bearer tokens, looks up `AgentCredential` entities, verifies the
//! linked `AgentType` is active, and returns a `ResolvedIdentity` that the
//! security context uses as the authoritative agent identity.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use crate::identity::error::{IdentityError, classify_dependency_read};
use crate::identity::jwt;
use crate::identity::principal_records::principal_records;
use crate::identity::resolver_support::{
    entity_state, hash_token, looks_like_jwt, parse_credential_expiry, require_field,
    same_credential_authority,
};
use crate::identity::revocation::credential_revoked;
use crate::state::ServerState;

/// Maximum opaque credential size accepted by the identity boundary.
pub const MAX_CREDENTIAL_BYTES: usize = 8 * 1024;
/// Clock-skew leeway for JWT exp/nbf validation (seconds).
const JWT_LEEWAY_SECS: i64 = 60;

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
    /// JWT path only: the owning human (agent's `acting_for` / token `sub`).
    pub acting_for: Option<String>,
    /// True when this identity came from a verified trusted-issuer JWT rather
    /// than the AgentCredential registry.
    pub from_jwt: bool,
    /// True when the principal is the human themselves (token carried only a
    /// `sub`, no `agent_type`) — a Cedar Customer, not an Agent.
    pub is_human: bool,
    /// Verified `role` claim (Cedar), when the issuer stamped one.
    pub role: Option<String>,
    /// JWT agents only: the verified `agent_label` claim, the name of this
    /// agent instance. Left out when empty; a person's token never gives one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// JWT path only: the statuses of the `principal_record` records that
    /// describe the user behind the token, keyed by the Cedar principal
    /// attribute each becomes (`<Type>` in person, `actingFor<Type>` for an
    /// agent's user). Read through each record's actor on every request.
    #[serde(default)]
    pub principal_records: BTreeMap<String, String>,
    /// JWT agents only: what the token was issued under, kept on the agent's
    /// security context so a timer it sets can ask, when it fires, whether
    /// the token would still be accepted
    /// ([`temper_authz::SecurityContext::credential`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<temper_authz::IssuedCredential>,
}

/// Resolves bearer tokens to platform-assigned agent identities.
///
/// Each protected request reads the credential and its linked agent type as
/// the entities they are, through their actors, the same way sign-out
/// generations and trusted issuers are read. Successful identities are not
/// cached, so a revocation or type deprecation takes effect on the next
/// request to this server. Freshness across several servers is a property of
/// all entities, not of this one type (ADR-0157 amendment).
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
    ///
    /// Returns [`IdentityError::Unavailable`] — never [`IdentityError::Invalid`]
    /// — when a dependency read failed rather than confirmed the credential
    /// invalid; see [`classify_dependency_read`].
    pub async fn resolve(
        &self,
        state: &ServerState,
        tenant: &TenantId,
        bearer_token: &str,
    ) -> Result<ResolvedIdentity, IdentityError> {
        if bearer_token.is_empty() || bearer_token.len() > MAX_CREDENTIAL_BYTES {
            return Err(IdentityError::Invalid);
        }

        // JWS-shaped tokens are verified against a registered TrustedIssuer;
        // opaque tokens are looked up in the AgentCredential registry.
        if looks_like_jwt(bearer_token) {
            return self.resolve_jwt(state, tenant, bearer_token).await;
        }

        let key_hash = hash_token(bearer_token);

        // Look up AgentCredential entity. We use the key_hash as entity ID
        // for O(1) lookup — the Issue action must use the key_hash as the
        // entity ID when creating credentials.
        let credential = classify_dependency_read(
            entity_state(state, tenant, "AgentCredential", &key_hash).await,
            "AgentCredential read",
        )?;

        // Verify credential is Active.
        if credential.status != "Active" {
            return Err(IdentityError::Invalid);
        }

        let fields = &credential.fields;
        let credential_expires_at = match parse_credential_expiry(fields) {
            Ok(expires_at) => expires_at,
            Err(error) => {
                tracing::warn!(tenant = %tenant, %error, "credential has invalid expiration metadata");
                return Err(IdentityError::Invalid);
            }
        };
        if credential_expires_at.is_some_and(|expires_at| sim_now() >= expires_at) {
            return Err(IdentityError::Invalid);
        }
        let agent_type_id = require_field(fields, "agent_type_id")?;
        let agent_instance_id = require_field(fields, "agent_instance_id")?;
        let stored_key_hash = require_field(fields, "key_hash")?;

        if agent_type_id.is_empty() || agent_instance_id.is_empty() || stored_key_hash != key_hash {
            return Err(IdentityError::Invalid);
        }

        // Look up linked AgentType entity.
        let agent_type = classify_dependency_read(
            entity_state(state, tenant, "AgentType", agent_type_id).await,
            "AgentType read",
        )?;

        // Verify AgentType is Active.
        if agent_type.status != "Active" {
            return Err(IdentityError::Invalid);
        }

        let agent_type_name = agent_type
            .fields
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|name| !name.is_empty())
            .ok_or(IdentityError::Invalid)?
            .to_string();

        // The credential and linked type are separate actors/streams. Re-read
        // the credential after observing the type and require an identical
        // authority-bearing snapshot. This establishes a point during the type
        // read at which both were active; without it, a revocation or link
        // change between the two reads could assemble a mixed-time identity
        // that never existed.
        let credential_recheck = classify_dependency_read(
            entity_state(state, tenant, "AgentCredential", &key_hash).await,
            "AgentCredential recheck",
        )?;
        if !same_credential_authority(&credential, &credential_recheck) {
            tracing::warn!(
                tenant = %tenant,
                credential = %key_hash,
                "credential authority changed during identity resolution"
            );
            return Err(IdentityError::Invalid);
        }

        let identity = ResolvedIdentity {
            agent_instance_id: agent_instance_id.to_string(),
            agent_type_id: agent_type_id.to_string(),
            agent_type_name,
            verified: true,
            acting_for: None,
            from_jwt: false,
            is_human: false,
            role: None,
            label: None,
            principal_records: BTreeMap::new(),
            credential: None,
        };

        // Re-check after the linked AgentType lookup so a short-lived
        // credential cannot cross its expiry while resolution is in flight.
        if credential_expires_at.is_some_and(|expires_at| sim_now() >= expires_at) {
            return Err(IdentityError::Invalid);
        }

        Ok(identity)
    }

    /// JWT path: verify an ES256 token against its registered `TrustedIssuer`,
    /// then map verified claims to an identity. No caching — every call
    /// re-reads current entity state, matching the credential path.
    async fn resolve_jwt(
        &self,
        state: &ServerState,
        tenant: &TenantId,
        token: &str,
    ) -> Result<ResolvedIdentity, IdentityError> {
        // Read `iss` from the unverified payload to pick the issuer entity.
        // Local parsing of attacker-controlled bytes — a failure here is a
        // malformed token, not a dependency problem.
        let unverified =
            jwt::decode_claims_unverified(token).map_err(|_| IdentityError::Invalid)?;
        let issuer_id = unverified.iss;

        // `iss` is attacker-chosen and unverified, and entity reads spawn the
        // entity on demand — so reading it directly would let anyone presenting
        // a junk token persist an Active TrustedIssuer row with empty fields.
        // Check existence in the index first and never materialise one from a
        // token. The index is made complete for the type once per process: an
        // issuer registered through the API is in the store but not in the
        // index after a restart (only the environment's issuer is registered
        // again at boot), so the index alone would deny its tokens then.
        state
            .ensure_type_indexed(tenant, "TrustedIssuer")
            .await
            .map_err(|error| {
                IdentityError::Unavailable(format!("TrustedIssuer index unavailable: {error}"))
            })?;
        if !state.entity_exists(tenant, "TrustedIssuer", &issuer_id) {
            return Err(IdentityError::Invalid);
        }
        let issuer = classify_dependency_read(
            entity_state(state, tenant, "TrustedIssuer", &issuer_id).await,
            "TrustedIssuer read",
        )?;
        if issuer.status != "Active" {
            return Err(IdentityError::Invalid);
        }

        let fields = &issuer.fields;
        let jwks_json = require_field(fields, "jwks_json")?;
        let audience = require_field(fields, "audience")?;
        let jwks: jwt::Jwks =
            serde_json::from_str(jwks_json).map_err(|_| IdentityError::Invalid)?;

        let now_unix = sim_now().timestamp();
        let claims = jwt::verify(
            token,
            &jwks,
            &issuer_id,
            audience,
            now_unix,
            JWT_LEEWAY_SECS,
        )
        .map_err(|_| IdentityError::Invalid)?;

        // "service" is the kernel's own role: the kernel treats a principal
        // with it as internal (`EventOrigin::from_caller`, `names_a_caller`),
        // so no token may claim it.
        if claims.role.as_deref() == Some("service") {
            return Err(IdentityError::Invalid);
        }

        // Refuse a token whose user signed out everywhere after it was issued
        // (keyed on the human `sub`, so it stops the agents acting for them
        // too) or whose grant was revoked: a revoked grant must stop working
        // at the kernel, not just at the MCP front door. A missing
        // `auth_generation` is generation 0, so an issuer that fails to stamp
        // it cannot skip revocation. `?` propagates Unavailable when a
        // generation read failed.
        let credential = temper_authz::IssuedCredential {
            subject: claims.sub.clone(),
            auth_generation: claims.auth_generation.unwrap_or(0),
            grant_id: claims.grant_id.clone().filter(|grant| !grant.is_empty()),
        };
        if credential_revoked(state, tenant, &credential)
            .await?
            .is_some()
        {
            return Err(IdentityError::Invalid);
        }

        // A token with an `agent_type` is an agent acting for the human `sub`;
        // a token with only a `sub` is the human themselves (a Customer).
        let identity = match claims.agent_type.as_deref().filter(|s| !s.is_empty()) {
            Some(agent_type) => {
                let client_id = claims.client_id.clone().unwrap_or_default();
                if client_id.is_empty() {
                    return Err(IdentityError::Invalid);
                }
                ResolvedIdentity {
                    agent_instance_id: client_id,
                    agent_type_id: String::new(),
                    agent_type_name: agent_type.to_string(),
                    verified: true,
                    acting_for: claims.sub.clone(),
                    from_jwt: true,
                    is_human: false,
                    role: claims.role.clone(),
                    label: claims.agent_label.clone().filter(|label| !label.is_empty()),
                    principal_records: principal_records(
                        state,
                        tenant,
                        None,
                        claims.sub.as_deref().filter(|sub| !sub.is_empty()),
                    )
                    .await?,
                    credential: Some(credential),
                }
            }
            None => {
                let sub = claims.sub.clone().unwrap_or_default();
                if sub.is_empty() {
                    return Err(IdentityError::Invalid);
                }
                ResolvedIdentity {
                    principal_records: principal_records(state, tenant, Some(&sub), None).await?,
                    agent_instance_id: sub,
                    agent_type_id: String::new(),
                    agent_type_name: String::new(),
                    verified: true,
                    acting_for: None,
                    from_jwt: true,
                    is_human: true,
                    role: claims.role.clone(),
                    label: None,
                    credential: None,
                }
            }
        };
        Ok(identity)
    }
}
