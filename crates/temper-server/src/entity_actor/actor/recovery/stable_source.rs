//! Stable snapshot/journal source capture shared by reads and index repair.

use super::*;

const MAX_STABLE_SOURCE_ATTEMPTS: usize = 3;

/// Exact snapshot generation that participated in state reconstruction.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CapturedEntitySnapshot {
    /// Snapshot sequence stored alongside its bytes.
    pub(crate) sequence_nr: u64,
    /// Exact serialized snapshot payload.
    pub(crate) state: Vec<u8>,
}

/// Entity state reconstructed from one stable durable source generation.
pub(crate) struct StableEntitySource {
    /// `None` proves that neither a journal nor a snapshot exists.
    pub(crate) state: Option<EntityState>,
    /// Exact journal high-water replayed into `state`.
    pub(crate) journal_sequence: u64,
    /// Snapshot generation used either as the complete snapshot-only state or as
    /// the legacy field baseline for a journal-backed state.
    pub(crate) snapshot: Option<CapturedEntitySnapshot>,
}

impl StableEntitySource {
    /// Newest durable sequence represented by this source generation.
    pub(crate) fn durable_sequence(&self) -> u64 {
        self.journal_sequence.max(
            self.snapshot
                .as_ref()
                .map(|snapshot| snapshot.sequence_nr)
                .unwrap_or(0),
        )
    }
}

async fn load_snapshot_strict(
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
    store: &BoxedEventStore,
    initial_fields: &serde_json::Value,
    persistence_id: &str,
) -> Result<Option<(CapturedEntitySnapshot, EntityState)>, ActorError> {
    let Some((sequence_nr, bytes)) =
        store.load_snapshot(persistence_id).await.map_err(|error| {
            ActorError::custom(format!(
                "failed to read durable snapshot for {entity_type}:{entity_id}: {error}"
            ))
        })?
    else {
        return Ok(None);
    };
    let mut state = EntityActor::build_initial_state(entity_type, entity_id, table, initial_fields);
    if !EntityActor::apply_snapshot_bytes(&mut state, sequence_nr, &bytes) {
        return Err(ActorError::custom(format!(
            "incompatible durable snapshot schema for {entity_type}:{entity_id} at sequence {sequence_nr}"
        )));
    }
    if state.entity_type != entity_type || state.entity_id != entity_id {
        return Err(ActorError::custom(format!(
            "durable snapshot identity mismatch for {entity_type}:{entity_id} at sequence {sequence_nr}"
        )));
    }
    Ok(Some((
        CapturedEntitySnapshot {
            sequence_nr,
            state: bytes,
        },
        state,
    )))
}

/// Revalidate a captured source generation without materializing or mutating it.
pub(crate) async fn stable_entity_source_is_current(
    store: &BoxedEventStore,
    persistence_id: &str,
    source: &StableEntitySource,
) -> Result<bool, ActorError> {
    let boundary = store
        .journal_boundary(persistence_id)
        .await
        .map_err(|error| {
            ActorError::custom(format!(
                "failed to close durable journal source {persistence_id}: {error}"
            ))
        })?;
    if boundary.latest_sequence != source.journal_sequence {
        return Ok(false);
    }
    let current_snapshot = store.load_snapshot(persistence_id).await.map_err(|error| {
        ActorError::custom(format!(
            "failed to close durable snapshot source {persistence_id}: {error}"
        ))
    })?;
    Ok(match (&source.snapshot, current_snapshot) {
        (None, None) => true,
        (Some(expected), Some((sequence_nr, state))) => {
            expected.sequence_nr == sequence_nr && expected.state == state
        }
        _ => false,
    })
}

/// Recover one entity from a stable snapshot/journal source generation.
///
/// A snapshot-only generation is materialized directly. Once a journal exists,
/// its full history owns lifecycle and current deltas, while a valid snapshot's
/// fields provide the legacy baseline for values older envelopes did not record.
/// Both sources are re-read byte-for-byte/high-water-for-high-water before return.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn recover_entity_state_from_stable_sources(
    tenant: &str,
    entity_type: &str,
    entity_id: &str,
    table: &TransitionTable,
    store: &BoxedEventStore,
    backend: BackendLabel,
    initial_fields: &serde_json::Value,
    blob_store: Option<&crate::blob_store::BlobStore>,
) -> Result<StableEntitySource, ActorError> {
    let persistence_id = format!("{tenant}:{entity_type}:{entity_id}");
    for _attempt in 1..=MAX_STABLE_SOURCE_ATTEMPTS {
        let initial_boundary = store
            .journal_boundary(&persistence_id)
            .await
            .map_err(|error| {
                ActorError::custom(format!(
                    "failed to read durable journal source for {entity_type}:{entity_id}: {error}"
                ))
            })?;
        let loaded_snapshot = load_snapshot_strict(
            entity_type,
            entity_id,
            table,
            store,
            initial_fields,
            &persistence_id,
        )
        .await?;
        let (snapshot, snapshot_state) = match loaded_snapshot {
            Some((snapshot, state)) => (Some(snapshot), Some(state)),
            None => (None, None),
        };

        let source = if initial_boundary.latest_sequence == 0 {
            StableEntitySource {
                state: snapshot_state,
                journal_sequence: 0,
                snapshot,
            }
        } else {
            let journal_initial_fields = snapshot_state
                .as_ref()
                .map(|state| state.fields.clone())
                .unwrap_or_else(|| initial_fields.clone());
            let state = recover_entity_state_from_journal_through(
                tenant,
                entity_type,
                entity_id,
                table,
                store,
                backend,
                &journal_initial_fields,
                blob_store,
                initial_boundary,
            )
            .await?;
            StableEntitySource {
                journal_sequence: state.sequence_nr,
                state: Some(state),
                snapshot,
            }
        };
        if stable_entity_source_is_current(store, &persistence_id, &source).await? {
            return Ok(source);
        }
    }
    Err(ActorError::custom(format!(
        "durable source generation for {entity_type}:{entity_id} did not stabilize after {MAX_STABLE_SOURCE_ATTEMPTS} attempts"
    )))
}
