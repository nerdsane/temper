//! Atomic multi-journal append data types.

use serde::{Deserialize, Serialize};

use super::{EntityKeyRow, PersistenceEnvelope, SnapshotSourceFence};

/// Stable, content-bound identity for one atomic multi-journal operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistenceBatchIdempotency {
    /// Durable namespace for the claim, normally the composite parent stream.
    pub persistence_id: String,
    /// Caller-supplied idempotency key within `persistence_id`.
    pub idempotency_key: String,
    /// Digest of the complete intended batch; a reused key with different work fails.
    pub intent_hash: String,
}

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
    /// Exact snapshot generation used to derive this stream's state and key rows.
    #[serde(default)]
    pub snapshot_source: SnapshotSourceFence,
    /// Optional claim co-committed with the whole append batch.
    ///
    /// Exactly zero or one append in a batch may carry this value. Stores use
    /// it to recognize a committed retry without scanning an unbounded journal.
    #[serde(default)]
    pub batch_idempotency: Option<PersistenceBatchIdempotency>,
}

/// New sequence number for one stream after an atomic batch append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistenceAppendResult {
    /// Persistence ID that was appended.
    pub persistence_id: String,
    /// New highest sequence number for this journal.
    pub sequence_nr: u64,
    /// Whether the store recognized an already-committed batch claim and made no mutations.
    #[serde(default)]
    pub batch_already_applied: bool,
}
