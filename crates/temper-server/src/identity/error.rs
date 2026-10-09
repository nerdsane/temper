//! Typed identity-resolution outcomes.
//!
//! Bearer-token resolution must distinguish a genuinely invalid credential
//! from an authoritative identity authority that could not be read at all.
//! Collapsing the two into one `None` makes an outage look identical to a
//! bad token, which is exactly the fail-open shape this type exists to rule
//! out: every consumer maps [`IdentityError::Unavailable`] to a dependency
//! failure (HTTP 503) and [`IdentityError::Invalid`] to the existing
//! 401/403/404 contract.

use std::fmt;

/// Why bearer-token resolution did not produce a verified identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityError {
    /// The credential, its linked type, or an issuer/governance record is
    /// genuinely missing, revoked, expired, malformed, or scoped to a
    /// different tenant. Denied exactly as before the credential is still
    /// invalid, not merely unverifiable.
    Invalid,
    /// A read the resolution depends on — credential, linked type, issuer,
    /// generation counter, or the governing spec registry itself — could
    /// not be completed, or came back corrupt. The identity authority is
    /// unavailable; this must never be reported as an invalid or absent
    /// credential.
    Unavailable(String),
}

impl fmt::Display for IdentityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IdentityError::Invalid => write!(f, "credential invalid"),
            IdentityError::Unavailable(reason) => {
                write!(f, "identity authority unavailable: {reason}")
            }
        }
    }
}

impl std::error::Error for IdentityError {}

/// Classify a dependency read into a typed identity outcome.
///
/// `Err` means the read itself failed before existence could even be
/// determined — the authority is unavailable, not that the record is
/// missing — and must classify as [`IdentityError::Unavailable`]. `Ok(None)`
/// means the read succeeded and positively confirmed the record does not
/// exist, the only outcome allowed to classify as [`IdentityError::Invalid`].
///
/// Every resolver call site that reads a dependency (`AgentCredential`,
/// `AgentType`, the credential recheck, `TrustedIssuer`, `PrincipalGeneration`,
/// and the governing spec registry) goes through this one function so the
/// Err-vs-absent distinction is enforced in exactly one place.
pub(crate) fn classify_dependency_read<T, E: fmt::Display>(
    read: Result<Option<T>, E>,
    what: &str,
) -> Result<T, IdentityError> {
    match read {
        Ok(Some(value)) => Ok(value),
        Ok(None) => Err(IdentityError::Invalid),
        Err(error) => Err(IdentityError::Unavailable(format!(
            "{what} unavailable: {error}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_dependency_read_maps_err_to_unavailable() {
        let read: Result<Option<i32>, String> = Err("store offline".to_string());
        assert_eq!(
            classify_dependency_read(read, "AgentCredential read"),
            Err(IdentityError::Unavailable(
                "AgentCredential read unavailable: store offline".to_string()
            ))
        );
    }

    #[test]
    fn classify_dependency_read_maps_confirmed_absence_to_invalid() {
        let read: Result<Option<i32>, String> = Ok(None);
        assert_eq!(
            classify_dependency_read(read, "AgentType read"),
            Err(IdentityError::Invalid)
        );
    }

    #[test]
    fn classify_dependency_read_passes_through_found_value() {
        let read: Result<Option<i32>, String> = Ok(Some(42));
        assert_eq!(classify_dependency_read(read, "credential recheck"), Ok(42));
    }

    // Every resolver call site is covered individually here, keyed on its
    // exact `what` string, because end-to-end fault injection does not reach
    // all of them (see `IdentityResolver::resolve_jwt`/`current_generation`:
    // the TrustedIssuer and PrincipalGeneration reads go through the
    // actor-ask path, which has no deterministic fault-injection fixture in
    // this workspace today — only the store-replay path does, via
    // `SimEventStore::fail_next_reads`). This is the closest classification-
    // level coverage available for those two call sites without adding new
    // actor fault-injection plumbing, which is out of scope for this change.
    #[test]
    fn classify_dependency_read_covers_every_resolver_call_site() {
        let unavailable = |what: &str| -> Result<i32, IdentityError> {
            let read: Result<Option<i32>, String> = Err("read failed".to_string());
            classify_dependency_read(read, what)
        };
        for what in [
            "AgentCredential read",
            "AgentType read",
            "AgentCredential recheck",
            "TrustedIssuer read",
            "PrincipalGeneration read",
        ] {
            assert_eq!(
                unavailable(what),
                Err(IdentityError::Unavailable(format!(
                    "{what} unavailable: read failed"
                ))),
                "call site '{what}' must classify a dependency read failure as Unavailable"
            );
        }
    }

    #[test]
    fn unavailable_error_message_never_claims_invalidity() {
        let error = IdentityError::Unavailable("replay failed".to_string());
        let message = error.to_string();
        assert!(
            !message.contains("invalid") && !message.contains("missing"),
            "unavailable wording must not imply the credential itself was bad: {message}"
        );
    }
}
