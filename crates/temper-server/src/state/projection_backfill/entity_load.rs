//! Durable source selection for projection backfill.

use temper_runtime::tenant::TenantId;

use crate::entity_actor::{
    CapturedEntitySnapshot, EntityRecoveryContext, StableEntitySource,
    recover_entity_state_from_stable_sources,
};

/// Outcome of loading one entity's current state for an index backfill (ADR-0153,
/// ADR-0155). Shared by the key and vector backfills so they classify entities the
/// same way — the distinction is the watermark soundness gate.
pub(super) enum EntityLoadOutcome {
    /// Loaded — index it from these fields, fenced at the replayed journal sequence.
    Fields {
        fields: serde_json::Value,
        sequence_nr: u64,
        journal_sequence: u64,
        snapshot: Option<CapturedEntitySnapshot>,
    },
    /// Durably deleted. Tombstone replay is authoritative even if an asynchronous
    /// catalog row still contains the entity's former live fields.
    Deleted {
        sequence_nr: u64,
        journal_sequence: u64,
        snapshot: Option<CapturedEntitySnapshot>,
    },
    /// No replayable events or valid snapshot state. This can be a true key-only
    /// phantom, or a migration-era entity whose catalog is its durable state.
    Missing {
        sequence_nr: u64,
        journal_sequence: u64,
        snapshot: Option<CapturedEntitySnapshot>,
    },
    /// The entity exists (it was enumerated from the durable store) but its current
    /// state could not be loaded — no transition table to replay with, an unreadable
    /// snapshot, or a replay error. Indexing it is impossible, so the type must NOT be
    /// watermarked; otherwise a read would treat a present-but-unindexed entity as
    /// authoritatively covered. This is the soundness gate.
    LoadFailed,
}

/// Load one entity's CURRENT state plus the exact snapshot/journal generation that
/// must remain unchanged until the index row is reconciled.
pub(super) async fn load_entity_current_fields(
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    table: Option<&temper_jit::TransitionTable>,
    store: &crate::storage::BoxedEventStore,
    backend: crate::storage::BackendLabel,
    blob_store: Option<&crate::blob_store::BlobStore>,
) -> EntityLoadOutcome {
    let Some(table) = table else {
        return EntityLoadOutcome::LoadFailed;
    };
    let source: StableEntitySource =
        match recover_entity_state_from_stable_sources(EntityRecoveryContext {
            tenant: tenant.as_str(),
            entity_type,
            entity_id,
            table,
            store,
            backend,
            initial_fields: &serde_json::json!({}),
            blob_store,
        })
        .await
        {
            Ok(source) => source,
            Err(_) => return EntityLoadOutcome::LoadFailed,
        };
    let sequence_nr = source.durable_sequence();
    let journal_sequence = source.journal_sequence;
    let snapshot = source.snapshot;
    match source.state {
        None => EntityLoadOutcome::Missing {
            sequence_nr,
            journal_sequence,
            snapshot,
        },
        Some(state) if state.status == "Deleted" => EntityLoadOutcome::Deleted {
            sequence_nr,
            journal_sequence,
            snapshot,
        },
        Some(state) if state.total_event_count == 0 && snapshot.is_none() => {
            EntityLoadOutcome::Missing {
                sequence_nr,
                journal_sequence,
                snapshot,
            }
        }
        Some(state) => EntityLoadOutcome::Fields {
            fields: state.fields,
            sequence_nr,
            journal_sequence,
            snapshot,
        },
    }
}
