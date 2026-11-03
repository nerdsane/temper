//! Whether a verified token would still be accepted: its user has not signed
//! out everywhere since it was issued, and its grant has not been revoked.
//!
//! The request path asks this of every trusted-issuer token. A timer an agent
//! set asks it of the token the agent's identity was resolved from, kept on
//! that identity ([`IssuedCredential`]), when the timer fires. Both read the
//! same `PrincipalGeneration` counters the same way, so a token the request
//! path would refuse never acts through a timer either.

use std::fmt;

use temper_authz::IssuedCredential;
use temper_runtime::tenant::TenantId;

use crate::identity::error::{IdentityError, classify_dependency_read};
use crate::identity::resolver_support::entity_state;
use crate::state::ServerState;

/// Why a token issued under a credential would no longer be accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Revoked {
    /// Its user signed out everywhere after it was issued: their generation
    /// is past the one the token carries.
    SignedOut,
    /// Its grant was revoked: the grant's generation is past 0.
    GrantRevoked,
}

impl fmt::Display for Revoked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SignedOut => write!(f, "its user signed out everywhere after it was issued"),
            Self::GrantRevoked => write!(f, "its grant was revoked"),
        }
    }
}

/// Why a token issued under `credential` would be refused now, or None when
/// it would still be accepted.
///
/// # Errors
///
/// [`IdentityError::Unavailable`] when a generation could not be read: the
/// caller must refuse to act on the token as a dependency failure, never take
/// the read as "not revoked". [`IdentityError::Invalid`] when a generation
/// the index lists could not be found.
pub(crate) async fn credential_revoked(
    state: &ServerState,
    tenant: &TenantId,
    credential: &IssuedCredential,
) -> Result<Option<Revoked>, IdentityError> {
    // Sign-out everywhere (ARN-255 option A), keyed on the user's `sub`, so
    // signing a user out also stops the agents acting for them.
    if let Some(subject) = credential.subject.as_deref()
        && credential.auth_generation < current_generation(state, tenant, subject).await?
    {
        return Ok(Some(Revoked::SignedOut));
    }
    // Grant liveness: the same counter keyed by `grant_id`. Any bump (> 0)
    // revokes, since a grant is never re-issued under the same id.
    if let Some(grant_id) = credential.grant_id.as_deref().filter(|id| !id.is_empty())
        && current_generation(state, tenant, grant_id).await? > 0
    {
        return Ok(Some(Revoked::GrantRevoked));
    }
    Ok(None)
}

/// Current monotonic generation for a principal/grant key.
///
/// `Err(Unavailable)` means the read failed and the caller must DENY
/// (fail closed) as a dependency failure, never as a stale/never-revoked
/// token. `Ok(0)` means the counter has never been bumped (never
/// revoked) without materialising a row.
async fn current_generation(
    state: &ServerState,
    tenant: &TenantId,
    key: &str,
) -> Result<i64, IdentityError> {
    // The index is made complete for the type once per process, so a
    // generation bumped before a restart still revokes after it, and the
    // common case (never revoked, no row) stays an index check with no
    // store read. An index that cannot be made complete fails closed.
    state
        .ensure_type_indexed(tenant, "PrincipalGeneration")
        .await
        .map_err(|error| {
            IdentityError::Unavailable(format!("PrincipalGeneration index unavailable: {error}"))
        })?;
    if !state.entity_exists(tenant, "PrincipalGeneration", key) {
        return Ok(0);
    }
    let generation = classify_dependency_read(
        entity_state(state, tenant, "PrincipalGeneration", key).await,
        "PrincipalGeneration read",
    )?;
    Ok(generation
        .counters
        .get("generation")
        .map(|c| *c as i64)
        .unwrap_or(0))
}
