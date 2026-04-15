//! Error types for the temper-observe crate.

use thiserror::Error;

/// Errors produced by observability store operations.
#[derive(Debug, Error)]
pub enum ObserveError {
    /// The SQL query was malformed or unsupported.
    #[error("invalid query: {0}")]
    InvalidQuery(String),

    /// A referenced column does not exist in the schema.
    #[error("unknown column: {0}")]
    UnknownColumn(String),

    /// The provider backend returned an error.
    #[error("provider error: {0}")]
    ProviderError(String),

    /// A connection or transport error occurred.
    #[error("connection error: {0}")]
    ConnectionError(String),

    /// Serialization or deserialization failed.
    #[error("serialization error: {0}")]
    SerializationError(#[from] serde_json::Error),

    /// A timeout was exceeded.
    #[error("timeout after {0}ms")]
    Timeout(u64),
}
