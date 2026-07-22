//! Derived key/vector index contracts shared by event-store backends.

use serde::{Deserialize, Serialize};

const ACTIVATED_KEY_CONTRACT_PREFIX: &str = "@temper-key-contract-v1:";

/// Attach the monotonic spec-activation epoch captured by a durable writer to
/// its stable declared-key signature.
pub fn encode_activated_key_contract(key_set: &str, activation_epoch: u64) -> String {
    format!(
        "{ACTIVATED_KEY_CONTRACT_PREFIX}{activation_epoch}:{}:{key_set}",
        key_set.len()
    )
}

/// Split a writer contract into its stable key-set signature and optional
/// activation epoch. Plain legacy/backfill signatures have no epoch.
pub fn decode_activated_key_contract(contract: &str) -> (&str, Option<u64>) {
    let Some(encoded) = contract.strip_prefix(ACTIVATED_KEY_CONTRACT_PREFIX) else {
        return (contract, None);
    };
    let Some((epoch, remainder)) = encoded.split_once(':') else {
        return (contract, None);
    };
    let Some((length, key_set)) = remainder.split_once(':') else {
        return (contract, None);
    };
    let Ok(epoch) = epoch.parse::<u64>() else {
        return (contract, None);
    };
    let Ok(length) = length.parse::<usize>() else {
        return (contract, None);
    };
    if key_set.len() != length {
        return (contract, None);
    }
    (key_set, Some(epoch))
}

/// One entity type's declared-key contract in an atomic spec activation batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyContractActivation {
    /// Entity type whose live table will receive the returned epoch.
    pub entity_type: String,
    /// Stable declared-key-set signature (without an activation epoch).
    pub key_set: String,
    /// Stable fingerprint of the full IOA source whose replay semantics derive
    /// key-property fields. A change forces coverage invalidation even when the
    /// declared key set itself is byte-identical.
    pub spec_fingerprint: String,
    /// Whether activation must atomically purge every prior ownership row for
    /// the type (the exact empty-contract transition).
    pub purge_existing_rows: bool,
}

/// A declared-key row to co-commit with an append (ADR-0153). The entity claims
/// `key_hash` for `key_name`; the store writes it into `entity_key_index` in the
/// same transaction as the journal append.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EntityKeyRow {
    /// The declared key's identifier (the `[[key]]` block's `name`).
    pub key_name: String,
    /// The canonical, type-tagged hash of the key's values.
    pub key_hash: String,
}

/// Authoritative owner and journal generation for one declared-key row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityKeyLookup {
    /// Entity that currently owns the declared key.
    pub entity_id: String,
    /// Owner journal sequence co-committed with this key row.
    pub sequence_nr: u64,
}

/// Contract and entity classification captured before a declared-key backfill
/// replays entity state. Every repair row must validate this fence before mutation
/// (ADR-0192).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyIndexBackfillFence<'a> {
    /// Versioned signature used to derive the attempted repair rows.
    pub key_set_signature: &'a str,
    /// Monotonic contract revision captured when the repair pass began.
    pub contract_revision: u64,
    /// Exact journal high-water observed before reconstructing the repair row.
    /// Revalidating this separately from the newest derived sequence prevents a
    /// catalog/snapshot-only owner at generation N from racing a first journal
    /// append at the same numeric generation N.
    pub expected_journal_sequence: u64,
    /// Whether authoritative enumeration classified the entity as live. The store
    /// revalidates this under the same stream lock as exact key reconciliation.
    pub expected_entity_live: bool,
    /// Exact snapshot generation whose legacy fields participated in state
    /// reconstruction. Presence, sequence, and bytes are revalidated together;
    /// `None` proves that no snapshot participated.
    pub expected_snapshot: Option<SnapshotBackfillFence<'a>>,
}

/// Exact derived snapshot generation used as a legacy baseline during key repair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotBackfillFence<'a> {
    /// Snapshot sequence captured before reconstruction.
    pub sequence_nr: u64,
    /// Exact serialized snapshot bytes captured before reconstruction.
    pub state: &'a [u8],
}

/// Exact journal and snapshot generation used to derive a query projection.
///
/// Projection repair must validate both components in the same storage
/// transaction that mutates the catalog and field index. Comparing only a
/// numeric sequence cannot distinguish a same-sequence snapshot replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionSourceFence<'a> {
    /// Exact journal high-water captured during state reconstruction.
    pub expected_journal_sequence: u64,
    /// Exact snapshot generation that participated in reconstruction. `None`
    /// proves that no snapshot was present.
    pub expected_snapshot: Option<SnapshotBackfillFence<'a>>,
}

/// Exact durable snapshot generation an event append was derived from.
///
/// `Unchecked` preserves compatibility for callers whose state does not depend on
/// snapshots. Entity actors and atomic composite writers must instead use
/// `Absent` or `Exact`, allowing the store to reject a same-sequence snapshot
/// replacement before journal and derived-index rows are mutated.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SnapshotSourceFence {
    /// The caller does not participate in snapshot-source fencing.
    #[default]
    Unchecked,
    /// State reconstruction observed no durable snapshot.
    Absent,
    /// State reconstruction used this exact snapshot sequence and payload.
    Exact {
        /// Snapshot sequence captured during state reconstruction.
        sequence_nr: u64,
        /// Exact serialized snapshot payload captured during reconstruction.
        state: Vec<u8>,
    },
}

/// A derived vector-index row to co-commit with an append (ADR-0155).
#[derive(Debug, Clone, PartialEq)]
pub struct EntityVectorRow {
    /// The declared vector path's identifier (the `[[vector]]` block's `name`).
    pub decl_name: String,
    /// The model tag that partitions this vector's space.
    pub model_tag: String,
    /// The float vector, exactly `dims` long.
    pub vector: Vec<f32>,
}

/// Which derived index families an append authoritatively reconciles to the supplied
/// exact row sets. A participating store ignores a row family when its flag is false;
/// when true, even an empty row set means "delete every prior row for this entity."
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IndexReconciliation {
    /// Reconcile the entity's complete declared-key ownership set (ADR-0192).
    pub keys: bool,
    /// Versioned signature of the complete declared-key contract used for this write.
    pub key_set_signature: Option<String>,
    /// Reconcile the entity's complete derived-vector row set (ADR-0155).
    pub vectors: bool,
    /// Exact snapshot generation used to derive the event and index rows.
    pub snapshot_source: SnapshotSourceFence,
}

/// Pack an `f32` slice to little-endian bytes for `entity_vector_index`.
pub fn pack_f32_le(vector: &[f32]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(vector.len() * 4);
    for value in vector {
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Unpack finite little-endian `f32` values, rejecting corrupt byte lengths and
/// non-finite components.
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

/// One entity/vector candidate returned from an index partition.
#[derive(Debug, Clone, PartialEq)]
pub struct EntityVectorCandidate {
    /// The entity holding this vector.
    pub entity_id: String,
    /// The float vector, exactly `dims` long.
    pub vector: Vec<f32>,
}
