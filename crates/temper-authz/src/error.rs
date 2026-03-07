//! Authorization error types.

/// Errors that can occur during authorization setup (policy parsing, engine init).
#[derive(Debug, thiserror::Error)]
pub enum AuthzError {
    /// The request was explicitly denied by policy.
    #[error("authorization denied: {0}")]
    Denied(String),

    /// A Cedar policy could not be parsed.
    #[error("policy parse error: {0}")]
    PolicyParse(String),

    /// The principal identifier was invalid.
    #[error("invalid principal: {0}")]
    InvalidPrincipal(String),

    /// The resource identifier was invalid.
    #[error("invalid resource: {0}")]
    InvalidResource(String),

    /// An internal authorization engine error occurred.
    #[error("authorization engine error: {0}")]
    Engine(String),
}

/// A typed authorization denial, distinguishing policy denials from
/// request construction errors and engine failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthzDenial {
    /// A Cedar policy explicitly denied the request.
    PolicyDenied {
        /// Policy IDs that contributed to the denial.
        policy_ids: Vec<String>,
    },
    /// No permit policy matched the request (Cedar default-deny).
    NoMatchingPermit,
    /// The principal UID could not be constructed.
    InvalidPrincipal(String),
    /// The action UID could not be constructed.
    InvalidAction(String),
    /// The resource UID could not be constructed.
    InvalidResource(String),
    /// The request context was malformed.
    InvalidContext(String),
    /// The authorization engine encountered an internal error.
    EngineError(String),
}

impl std::fmt::Display for AuthzDenial {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthzDenial::PolicyDenied { policy_ids } => {
                if policy_ids.is_empty() {
                    write!(f, "denied by policy")
                } else {
                    write!(f, "denied by policy: {}", policy_ids.join(", "))
                }
            }
            AuthzDenial::NoMatchingPermit => write!(f, "no matching permit policy"),
            AuthzDenial::InvalidPrincipal(msg) => write!(f, "invalid principal: {msg}"),
            AuthzDenial::InvalidAction(msg) => write!(f, "invalid action: {msg}"),
            AuthzDenial::InvalidResource(msg) => write!(f, "invalid resource: {msg}"),
            AuthzDenial::InvalidContext(msg) => write!(f, "invalid context: {msg}"),
            AuthzDenial::EngineError(msg) => write!(f, "authorization engine error: {msg}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authz_error_display_denied() {
        let err = AuthzError::Denied("not allowed".into());
        assert_eq!(err.to_string(), "authorization denied: not allowed");
    }

    #[test]
    fn authz_error_display_policy_parse() {
        let err = AuthzError::PolicyParse("bad syntax".into());
        assert_eq!(err.to_string(), "policy parse error: bad syntax");
    }

    #[test]
    fn authz_error_display_invalid_principal() {
        let err = AuthzError::InvalidPrincipal("unknown".into());
        assert_eq!(err.to_string(), "invalid principal: unknown");
    }

    #[test]
    fn authz_error_display_invalid_resource() {
        let err = AuthzError::InvalidResource("missing".into());
        assert_eq!(err.to_string(), "invalid resource: missing");
    }

    #[test]
    fn authz_error_display_engine() {
        let err = AuthzError::Engine("internal failure".into());
        assert_eq!(err.to_string(), "authorization engine error: internal failure");
    }

    #[test]
    fn denial_display_policy_denied_with_ids() {
        let d = AuthzDenial::PolicyDenied {
            policy_ids: vec!["p1".into(), "p2".into()],
        };
        assert_eq!(d.to_string(), "denied by policy: p1, p2");
    }

    #[test]
    fn denial_display_policy_denied_empty_ids() {
        let d = AuthzDenial::PolicyDenied {
            policy_ids: vec![],
        };
        assert_eq!(d.to_string(), "denied by policy");
    }

    #[test]
    fn denial_display_no_matching_permit() {
        assert_eq!(AuthzDenial::NoMatchingPermit.to_string(), "no matching permit policy");
    }

    #[test]
    fn denial_display_invalid_variants() {
        assert_eq!(
            AuthzDenial::InvalidPrincipal("bad".into()).to_string(),
            "invalid principal: bad"
        );
        assert_eq!(
            AuthzDenial::InvalidAction("nope".into()).to_string(),
            "invalid action: nope"
        );
        assert_eq!(
            AuthzDenial::InvalidResource("gone".into()).to_string(),
            "invalid resource: gone"
        );
        assert_eq!(
            AuthzDenial::InvalidContext("malformed".into()).to_string(),
            "invalid context: malformed"
        );
        assert_eq!(
            AuthzDenial::EngineError("crash".into()).to_string(),
            "authorization engine error: crash"
        );
    }

    #[test]
    fn denial_equality() {
        assert_eq!(AuthzDenial::NoMatchingPermit, AuthzDenial::NoMatchingPermit);
        assert_ne!(
            AuthzDenial::InvalidPrincipal("a".into()),
            AuthzDenial::InvalidAction("a".into())
        );
    }
}
