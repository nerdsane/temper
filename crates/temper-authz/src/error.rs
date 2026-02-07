//! Authorization error types.

/// Errors that can occur during authorization.
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
