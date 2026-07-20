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

/// Exact durable journal high-water and terminal lifecycle boundary for one stream.
///
/// Recovery uses both values together: `latest_sequence` proves that a replay did
/// not stop at a fault-truncated prefix, while `first_terminal_sequence` prevents a
/// newer derived snapshot from reviving a stream that was already deleted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JournalBoundary {
    /// Highest durable event sequence, or zero when the stream has no events.
    pub latest_sequence: u64,
    /// First event that durably transitioned the entity to `Deleted`, if any.
    pub first_terminal_sequence: Option<u64>,
}

impl PersistenceEnvelope {
    /// Whether this durable event makes the entity terminally deleted.
    ///
    /// Legacy writers used the canonical `Deleted` event/action name. Normal specs
    /// may instead name the action `Delete` (or anything else) while persisting the
    /// lifecycle boundary in `to_status`.
    pub fn transitions_to_deleted(&self) -> bool {
        match self.payload.get("to_status") {
            Some(serde_json::Value::String(status)) => status == "Deleted",
            // Structured lifecycle metadata is authoritative even when malformed:
            // legacy name inference is only valid when the field is absent.
            Some(_) => false,
            None => {
                self.event_type == "Deleted"
                    || self
                        .payload
                        .get("action")
                        .and_then(serde_json::Value::as_str)
                        == Some("Deleted")
            }
        }
    }
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
    /// A key-index repair's entity classification became stale before mutation.
    #[error(
        "entity liveness changed during key repair: expected live={expected_live}, got live={actual_live}"
    )]
    EntityLivenessChanged {
        /// Liveness observed by the repair pass before replay.
        expected_live: bool,
        /// Liveness observed under the store's stream lock.
        actual_live: bool,
    },
    /// A key-index repair's exact journal source boundary changed before mutation.
    #[error(
        "journal boundary changed during key repair: expected sequence {expected}, got {actual}"
    )]
    JournalBoundaryChanged {
        /// Journal high-water captured before state reconstruction.
        expected: u64,
        /// Journal high-water observed under the store's stream lock.
        actual: u64,
    },
    /// A key-index repair's exact snapshot source changed before mutation.
    #[error("snapshot generation changed during key repair")]
    SnapshotGenerationChanged,
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
