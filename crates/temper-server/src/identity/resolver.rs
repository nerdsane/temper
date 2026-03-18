//! Credential-to-identity resolution.
//!
//! Hashes bearer tokens, looks up `AgentCredential` entities, verifies the
//! linked `AgentType` is active, and returns a `ResolvedIdentity` that the
//! security context uses as the authoritative agent identity.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use crate::state::ServerState;

/// Cache entry TTL in seconds.
const CACHE_TTL_SECS: i64 = 60;

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

/// Cached resolution result with expiry.
struct CacheEntry {
    identity: ResolvedIdentity,
    expires_at: chrono::DateTime<chrono::Utc>,
}

/// Resolves bearer tokens to platform-assigned agent identities.
///
/// Uses an in-memory cache (`BTreeMap` for DST determinism) to avoid
/// entity lookups on every request. Cache entries expire after [`CACHE_TTL_SECS`].
pub struct IdentityResolver {
    cache: Arc<RwLock<BTreeMap<String, CacheEntry>>>,
}

impl Default for IdentityResolver {
    fn default() -> Self {
        Self::new()
    }
}

impl IdentityResolver {
    /// Create a new identity resolver.
    pub fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(BTreeMap::new())),
        }
    }

    /// Resolve a bearer token to a verified agent identity.
    ///
    /// 1. Hash the token (SHA-256)
    /// 2. Check cache (hit → return immediately)
    /// 3. Look up `AgentCredential` entity by using key_hash as entity ID
    /// 4. Verify credential is `Active`
    /// 5. Look up linked `AgentType` entity
    /// 6. Verify AgentType is `Active`
    /// 7. Cache and return `ResolvedIdentity`
    pub async fn resolve(
        &self,
        state: &ServerState,
        tenant: &TenantId,
        bearer_token: &str,
    ) -> Option<ResolvedIdentity> {
        let key_hash = hash_token(bearer_token);

        // Check cache first.
        if let Some(cached) = self.get_cached(&key_hash) {
            return Some(cached);
        }

        // Look up AgentCredential entity. We use the key_hash as entity ID
        // for O(1) lookup — the Issue action must use the key_hash as the
        // entity ID when creating credentials.
        let cred_response = state
            .get_tenant_entity_state(tenant, "AgentCredential", &key_hash)
            .await
            .ok()?;

        // Verify credential is Active.
        if cred_response.state.status != "Active" {
            return None;
        }

        let fields = &cred_response.state.fields;
        let agent_type_id = fields.get("agent_type_id")?.as_str()?;
        let agent_instance_id = fields.get("agent_instance_id")?.as_str()?;

        if agent_type_id.is_empty() || agent_instance_id.is_empty() {
            return None;
        }

        // Look up linked AgentType entity.
        let type_response = state
            .get_tenant_entity_state(tenant, "AgentType", agent_type_id)
            .await
            .ok()?;

        // Verify AgentType is Active.
        if type_response.state.status != "Active" {
            return None;
        }

        let agent_type_name = type_response
            .state
            .fields
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();

        let identity = ResolvedIdentity {
            agent_instance_id: agent_instance_id.to_string(),
            agent_type_id: agent_type_id.to_string(),
            agent_type_name,
            verified: true,
        };

        // Cache the result.
        self.put_cached(key_hash, identity.clone());

        Some(identity)
    }

    /// Invalidate all cached entries (e.g., after credential rotation/revocation).
    pub fn invalidate_all(&self) {
        let mut cache = self.cache.write().unwrap(); // ci-ok: infallible lock
        cache.clear();
    }

    /// Invalidate a specific credential by its token.
    pub fn invalidate_token(&self, bearer_token: &str) {
        let key_hash = hash_token(bearer_token);
        self.invalidate_key_hash(&key_hash);
    }

    /// Invalidate a specific credential by its hashed key (`AgentCredential` entity ID).
    pub fn invalidate_key_hash(&self, key_hash: &str) {
        let mut cache = self.cache.write().unwrap(); // ci-ok: infallible lock
        cache.remove(key_hash);
    }

    fn get_cached(&self, key_hash: &str) -> Option<ResolvedIdentity> {
        let cache = self.cache.read().unwrap(); // ci-ok: infallible lock
        let entry = cache.get(key_hash)?;
        let now = sim_now();
        if now < entry.expires_at {
            Some(entry.identity.clone())
        } else {
            None
        }
    }

    fn put_cached(&self, key_hash: String, identity: ResolvedIdentity) {
        let expires_at = sim_now() + chrono::Duration::seconds(CACHE_TTL_SECS);
        let mut cache = self.cache.write().unwrap(); // ci-ok: infallible lock

        // Evict expired entries opportunistically (bounded work: max 32 per insert).
        let now = sim_now();
        let expired_keys: Vec<String> = cache
            .iter()
            .filter(|(_, entry)| now >= entry.expires_at)
            .take(32)
            .map(|(k, _)| k.clone())
            .collect();
        for k in expired_keys {
            cache.remove(&k);
        }

        cache.insert(
            key_hash,
            CacheEntry {
                identity,
                expires_at,
            },
        );
    }
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
}
