//! Authorization error types.

#[derive(Debug, thiserror::Error)]
pub enum AuthzError {
    #[error("authorization denied: {0}")]
    Denied(String),

    #[error("policy parse error: {0}")]
    PolicyParse(String),

    #[error("invalid principal: {0}")]
    InvalidPrincipal(String),

    #[error("invalid resource: {0}")]
    InvalidResource(String),

    #[error("authorization engine error: {0}")]
    Engine(String),
}
