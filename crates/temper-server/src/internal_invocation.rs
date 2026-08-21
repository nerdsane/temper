//! Short-lived credentials for authenticated internal HTTP re-entry.
//!
//! A credential is an opaque, single-use capability. The store retains only a
//! SHA-256 digest and binds the capability to one tenant, HTTP method, exact
//! canonical path/query, and immutable authenticated request context.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::{Method, Uri};
use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind};
use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

/// Prefix reserved for internal invocation bearer credentials.
pub const INTERNAL_INVOCATION_BEARER_PREFIX: &str = "temper-internal-v1.";

/// Maximum number of outstanding internal invocation credentials.
pub const INTERNAL_INVOCATION_CREDENTIAL_CAPACITY: usize = 4_096;

/// Per-tenant share of the global credential budget.
pub const INTERNAL_INVOCATION_CREDENTIAL_TENANT_CAPACITY: usize = 256;

/// Lifetime of an internal invocation credential.
pub const INTERNAL_INVOCATION_CREDENTIAL_TTL: Duration = Duration::from_secs(30);

const TOKEN_GENERATION_ATTEMPT_BUDGET: usize = 8;

/// Injected clock used by the credential store.
pub type InternalInvocationNowFn = Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>;

/// Injected source of 32 bytes of opaque token material.
pub type InternalInvocationTokenFn = Arc<dyn Fn() -> [u8; 32] + Send + Sync>;

/// Errors returned while issuing or consuming an internal credential.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum InternalInvocationCredentialError {
    /// The supplied URL or HTTP method cannot be canonicalized.
    #[error("invalid internal invocation request target")]
    InvalidRequestTarget,
    /// The token is unknown, expired, malformed, replayed, or has the wrong prefix.
    #[error("invalid internal invocation credential")]
    InvalidCredential,
    /// The credential exists but is not bound to this request.
    #[error("internal invocation credential binding mismatch")]
    BindingMismatch,
    /// Kernel System authority cannot cross an HTTP bearer boundary.
    #[error("System authority cannot be delegated through internal HTTP")]
    SystemContextNotDelegable,
    /// The injected token source repeatedly generated an existing credential.
    #[error("internal invocation token generation budget exhausted")]
    TokenGenerationExhausted,
    /// No capacity remains that can be reclaimed from the issuing tenant.
    #[error("internal invocation credential capacity exhausted")]
    CapacityExhausted,
    /// The credential store lock was poisoned.
    #[error("internal invocation credential store unavailable")]
    StoreUnavailable,
}

#[derive(Clone)]
struct CredentialEntry {
    context: AuthenticatedRequestContext,
    tenant: TenantId,
    method: String,
    target: String,
    expires_at: DateTime<Utc>,
    issue_sequence: u64,
}

#[derive(Default)]
struct CredentialState {
    entries: BTreeMap<[u8; 32], CredentialEntry>,
    next_issue_sequence: u64,
}

/// Bounded store for short-lived, single-use internal invocation credentials.
#[derive(Clone)]
pub struct InternalInvocationCredentialStore {
    state: Arc<Mutex<CredentialState>>,
    capacity: usize,
    tenant_capacity: usize,
    ttl: Duration,
    now: InternalInvocationNowFn,
    token: InternalInvocationTokenFn,
}

impl InternalInvocationCredentialStore {
    /// Create the runtime store.
    ///
    /// The default simulation context supplies deterministic time and IDs.
    /// Production's `sim_uuid()` source is UUIDv7-backed; two independent UUIDs
    /// provide more than 128 random bits while still allowing deterministic
    /// source injection in DST.
    pub fn runtime() -> Self {
        Self::with_limits_and_sources(
            INTERNAL_INVOCATION_CREDENTIAL_CAPACITY,
            INTERNAL_INVOCATION_CREDENTIAL_TENANT_CAPACITY,
            INTERNAL_INVOCATION_CREDENTIAL_TTL,
            Arc::new(sim_now),
            Arc::new(|| {
                let first = sim_uuid();
                let second = sim_uuid();
                let mut bytes = [0_u8; 32];
                bytes[..16].copy_from_slice(first.as_bytes());
                bytes[16..].copy_from_slice(second.as_bytes());
                bytes
            }),
        )
    }

    /// Create a store with explicit bounds, clock, and token source.
    pub fn with_sources(
        capacity: usize,
        ttl: Duration,
        now: InternalInvocationNowFn,
        token: InternalInvocationTokenFn,
    ) -> Self {
        Self::with_limits_and_sources(capacity, capacity, ttl, now, token)
    }

    fn with_limits_and_sources(
        capacity: usize,
        tenant_capacity: usize,
        ttl: Duration,
        now: InternalInvocationNowFn,
        token: InternalInvocationTokenFn,
    ) -> Self {
        assert!(capacity > 0, "credential capacity must be positive");
        assert!(tenant_capacity > 0, "tenant capacity must be positive");
        assert!(
            tenant_capacity <= capacity,
            "tenant capacity must not exceed global capacity"
        );
        assert!(!ttl.is_zero(), "credential TTL must be positive");
        Self {
            state: Arc::new(Mutex::new(CredentialState::default())),
            capacity,
            tenant_capacity,
            ttl,
            now,
            token,
        }
    }

    /// Issue one credential bound to an absolute request URL.
    pub fn issue_for_url(
        &self,
        context: AuthenticatedRequestContext,
        method: &str,
        url: &str,
    ) -> Result<String, InternalInvocationCredentialError> {
        if context.security_context().principal.kind == PrincipalKind::System {
            return Err(InternalInvocationCredentialError::SystemContextNotDelegable);
        }
        let method = canonical_method(method)?;
        let target = canonical_request_target_from_url(url)?;
        let tenant = context.tenant().clone();
        let now = (self.now)();
        let expires_at = now
            + chrono::Duration::from_std(self.ttl)
                .map_err(|_| InternalInvocationCredentialError::InvalidRequestTarget)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| InternalInvocationCredentialError::StoreUnavailable)?;

        purge_expired(&mut state, now);
        reclaim_issuing_tenant_slot(&mut state, &tenant, self.capacity, self.tenant_capacity);
        if state.entries.len() >= self.capacity {
            return Err(InternalInvocationCredentialError::CapacityExhausted);
        }
        let generated = (0..TOKEN_GENERATION_ATTEMPT_BUDGET).find_map(|_| {
            let token = encode_token((self.token)());
            let digest = credential_digest(&token);
            if state.entries.contains_key(&digest) {
                None
            } else {
                Some((token, digest))
            }
        });
        let Some((token, digest)) = generated else {
            return Err(InternalInvocationCredentialError::TokenGenerationExhausted);
        };

        let issue_sequence = state.next_issue_sequence;
        state.next_issue_sequence = state.next_issue_sequence.wrapping_add(1);
        state.entries.insert(
            digest,
            CredentialEntry {
                context,
                tenant,
                method,
                target,
                expires_at,
                issue_sequence,
            },
        );
        debug_assert!(state.entries.len() <= self.capacity);
        Ok(token)
    }

    /// Consume a credential for the exact inbound request.
    ///
    /// Removal happens before binding validation, so every credential can be
    /// presented at most once even when the presenting request is malformed.
    pub fn consume_for_request(
        &self,
        token: &str,
        tenant: &TenantId,
        method: &Method,
        uri: &Uri,
    ) -> Result<AuthenticatedRequestContext, InternalInvocationCredentialError> {
        if !is_internal_invocation_bearer(token) {
            return Err(InternalInvocationCredentialError::InvalidCredential);
        }
        let digest = credential_digest(token);
        let now = (self.now)();
        let mut state = self
            .state
            .lock()
            .map_err(|_| InternalInvocationCredentialError::StoreUnavailable)?;
        let Some(entry) = state.entries.remove(&digest) else {
            return Err(InternalInvocationCredentialError::InvalidCredential);
        };

        if now >= entry.expires_at {
            return Err(InternalInvocationCredentialError::InvalidCredential);
        }
        if entry.context.security_context().principal.kind == PrincipalKind::System {
            return Err(InternalInvocationCredentialError::SystemContextNotDelegable);
        }
        let target = canonical_request_target_from_uri(uri);
        if entry.tenant != *tenant
            || entry.context.tenant() != tenant
            || entry.method != method.as_str()
            || entry.target != target
        {
            return Err(InternalInvocationCredentialError::BindingMismatch);
        }

        Ok(entry.context)
    }

    #[cfg(test)]
    fn len(&self) -> Result<usize, InternalInvocationCredentialError> {
        self.state
            .lock()
            .map(|state| state.entries.len())
            .map_err(|_| InternalInvocationCredentialError::StoreUnavailable)
    }
}

/// Return whether a bearer value uses the reserved internal prefix.
pub fn is_internal_invocation_bearer(token: &str) -> bool {
    token.starts_with(INTERNAL_INVOCATION_BEARER_PREFIX)
}

/// Canonicalize the path and query reqwest will send for an absolute URL.
pub fn canonical_request_target_from_url(
    url: &str,
) -> Result<String, InternalInvocationCredentialError> {
    let url = reqwest::Url::parse(url)
        .map_err(|_| InternalInvocationCredentialError::InvalidRequestTarget)?;
    let mut target = if url.path().is_empty() {
        "/".to_string()
    } else {
        url.path().to_string()
    };
    if let Some(query) = url.query() {
        target.push('?');
        target.push_str(query);
    }
    Ok(target)
}

/// Return the canonical path/query seen by the inbound HTTP router.
pub fn canonical_request_target_from_uri(uri: &Uri) -> String {
    uri.path_and_query()
        .map(|value| value.as_str().to_string())
        .unwrap_or_else(|| uri.path().to_string())
}

fn canonical_method(method: &str) -> Result<String, InternalInvocationCredentialError> {
    Method::from_bytes(method.as_bytes())
        .map(|method| method.as_str().to_string())
        .map_err(|_| InternalInvocationCredentialError::InvalidRequestTarget)
}

fn encode_token(bytes: [u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut opaque_hasher = Sha256::new();
    opaque_hasher.update(b"temper-internal-invocation-token-v1\0");
    opaque_hasher.update(bytes);
    let opaque: [u8; 32] = opaque_hasher.finalize().into();
    let mut token = String::with_capacity(INTERNAL_INVOCATION_BEARER_PREFIX.len() + 64);
    token.push_str(INTERNAL_INVOCATION_BEARER_PREFIX);
    for byte in opaque {
        token.push(HEX[(byte >> 4) as usize] as char);
        token.push(HEX[(byte & 0x0f) as usize] as char);
    }
    token
}

fn credential_digest(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn purge_expired(state: &mut CredentialState, now: DateTime<Utc>) {
    state.entries.retain(|_, entry| entry.expires_at > now);
}

fn reclaim_issuing_tenant_slot(
    state: &mut CredentialState,
    tenant: &TenantId,
    global_capacity: usize,
    tenant_capacity: usize,
) {
    let tenant_entries = state
        .entries
        .values()
        .filter(|entry| &entry.tenant == tenant)
        .count();
    if tenant_entries < tenant_capacity && state.entries.len() < global_capacity {
        return;
    }
    let oldest = state
        .entries
        .iter()
        .filter(|(_, entry)| &entry.tenant == tenant)
        .min_by_key(|(digest, entry)| (entry.expires_at, entry.issue_sequence, **digest))
        .map(|(digest, _)| *digest);
    if let Some(oldest) = oldest {
        state.entries.remove(&oldest);
    }
}

#[cfg(test)]
mod tests;
