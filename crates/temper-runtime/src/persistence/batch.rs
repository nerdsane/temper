//! Atomic multi-journal append data types.

use serde::{Deserialize, Serialize};

use super::{EntityKeyRow, PersistenceEnvelope};

/// One stream append inside an atomic multi-journal append.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistenceAppend {
    /// Persistence ID in the form `{tenant}:{entity_type}:{entity_id}`.
    pub persistence_id: String,
    /// Optimistic-concurrency sequence expected before this append.
    pub expected_sequence: u64,
    /// Events to append to this journal.
    pub events: Vec<PersistenceEnvelope>,
    /// The entity's complete current declared-key rows when `reconcile_keys` is true.
    #[serde(default)]
    pub key_rows: Vec<EntityKeyRow>,
    /// Whether the batch must replace this entity's complete declared-key row set.
    #[serde(default)]
    pub reconcile_keys: bool,
    /// Versioned declared-key signature used to derive `key_rows`.
    #[serde(default)]
    pub key_set_signature: Option<String>,
}

/// New sequence number for one stream after an atomic batch append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistenceAppendResult {
    /// Persistence ID that was appended.
    pub persistence_id: String,
    /// New highest sequence number for this journal.
    pub sequence_nr: u64,
}
