//! Persisted envelope and backend error types.

use serde::{Deserialize, Serialize};

use super::EventMetadata;

/// A persisted event with metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceEnvelope {
    /// Monotonic sequence number within the entity's journal.
    pub sequence_nr: u64,
    /// Fully qualified event type name.
    pub event_type: String,
    /// Serialized event payload.
    pub payload: serde_json::Value,
    /// Event metadata (causation, correlation, timestamp).
    pub metadata: EventMetadata,
}

/// Errors that can occur during event persistence operations.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    /// Optimistic concurrency check failed (another writer appended first).
    #[error("optimistic concurrency violation: expected sequence {expected}, got {actual}")]
    ConcurrencyViolation { expected: u64, actual: u64 },
    /// A key-index repair was derived under a type contract that is no longer current.
    #[error(
        "key contract changed: expected '{expected_signature}' at revision {expected_revision}, got {actual_signature:?} at revision {actual_revision}"
    )]
    KeyContractChanged {
        /// Signature used to derive the attempted repair rows.
        expected_signature: String,
        /// Contract revision captured before replay began.
        expected_revision: u64,
        /// Signature current when the repair acquired the type fence.
        actual_signature: Option<String>,
        /// Contract revision current when the repair acquired the type fence.
        actual_revision: u64,
    },
    /// Event serialization or deserialization failed.
    #[error("serialization error: {0}")]
    Serialization(String),
    /// Underlying storage backend returned an error.
    #[error("storage error: {0}")]
    Storage(String),
}

/// Convert backend-specific errors into [`PersistenceError::Storage`].
pub fn storage_error(err: impl std::fmt::Display) -> PersistenceError {
    PersistenceError::Storage(err.to_string())
}
