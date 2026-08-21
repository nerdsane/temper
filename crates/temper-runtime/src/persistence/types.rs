//! Shared event, index, and persistence value types.

use serde::{Deserialize, Serialize};

/// Event type used for the parent-journal record of a Composite action.
pub const COMPOSITE_EVENT_TYPE: &str = "CompositeEvent";

/// Replay/audit record for one Composite action application.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositeEvent {
    pub tenant: String,
    pub parent_entity_type: String,
    pub parent_entity_id: String,
    pub parent_action: String,
    pub composite_idempotency_key: String,
    pub sub_writes: Vec<CompositeEventSubWrite>,
}

/// One concrete sub-write recorded in a [`CompositeEvent`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompositeEventSubWrite {
    pub index: usize,
    pub entity_type: String,
    pub entity_id: String,
    pub action: String,
    pub idempotency_key: String,
}

/// Marker trait for serializable domain events.
pub trait DomainEvent:
    Send + Serialize + for<'de> Deserialize<'de> + std::fmt::Debug + 'static
{
}

/// Metadata attached to every persisted event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventMetadata {
    pub event_id: uuid::Uuid,
    pub causation_id: uuid::Uuid,
    pub correlation_id: uuid::Uuid,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub actor_id: String,
}

/// Trait for event-sourced persistent actors.
pub trait PersistentActor: Send + 'static {
    type Event: DomainEvent;
    type State: Send + Serialize + for<'de> Deserialize<'de> + 'static;

    fn persistence_id(&self) -> &str;
    fn apply_event(state: &mut Self::State, event: &Self::Event);
    fn snapshot_every(&self) -> u64 {
        100
    }
}

/// A declared-key row to co-commit with an append.
#[derive(Debug, Clone)]
pub struct EntityKeyRow {
    pub key_name: String,
    pub key_hash: String,
}

/// A derived vector-index row to co-commit with an append.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityVectorRow {
    pub decl_name: String,
    pub model_tag: String,
    pub vector: Vec<f32>,
}

/// Pack an `f32` slice to little-endian bytes.
pub fn pack_f32_le(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Unpack finite little-endian `f32` values.
pub fn unpack_f32_le(bytes: &[u8]) -> Option<Vec<f32>> {
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4);
    for chunk in bytes.chunks_exact(4) {
        let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !value.is_finite() {
            return None;
        }
        out.push(value);
    }
    Some(out)
}

/// One candidate row returned from the vector index.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityVectorCandidate {
    pub entity_id: String,
    pub vector: Vec<f32>,
}

/// A persisted event with metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistenceEnvelope {
    pub sequence_nr: u64,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub metadata: EventMetadata,
}

/// One stream append inside an atomic multi-journal append.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceAppend {
    pub persistence_id: String,
    pub expected_sequence: u64,
    pub events: Vec<PersistenceEnvelope>,
}

/// New sequence number for one stream after an atomic batch append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistenceAppendResult {
    pub persistence_id: String,
    pub sequence_nr: u64,
}

/// Errors that can occur during event persistence operations.
#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error("optimistic concurrency violation: expected sequence {expected}, got {actual}")]
    ConcurrencyViolation { expected: u64, actual: u64 },
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("storage error: {0}")]
    Storage(String),
}

/// Convert backend-specific errors into [`PersistenceError::Storage`].
pub fn storage_error(err: impl std::fmt::Display) -> PersistenceError {
    PersistenceError::Storage(err.to_string())
}
