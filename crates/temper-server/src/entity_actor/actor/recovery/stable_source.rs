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

#[derive(Default)]
pub(super) struct ReplaySummary {
    pub(super) replayed_state_materialization: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct CapturedReplaySource {
    pub(super) journal_boundary: JournalBoundary,
    pub(super) snapshot: Option<CapturedEntitySnapshot>,
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
    /// Whether strict replay applied the internal baseline that materializes a
    /// snapshot-only entity into the journal before non-domain audit records.
    pub(crate) replayed_state_materialization: bool,
}

impl StableEntitySource {
    /// Authoritative sequence represented by this source generation.
    ///
    /// A non-empty journal is a distinct generation whose coordinates start at
    /// one, so it owns the sequence even when a retired migration snapshot used
    /// a numerically larger coordinate. Snapshot sequence is authoritative only
    /// while the journal is empty.
    pub(crate) fn durable_sequence(&self) -> u64 {
        if self.journal_sequence > 0 {
            self.journal_sequence
        } else {
            self.snapshot
                .as_ref()
                .map(|snapshot| snapshot.sequence_nr)
                .unwrap_or(0)
        }
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
    if EntityActor::apply_snapshot_bytes(&mut state, sequence_nr, &bytes).is_none() {
        return Err(ActorError::custom(format!(
            "incompatible durable snapshot schema for {entity_type}:{entity_id} at sequence {sequence_nr}"
        )));
    }
    super::validate_and_normalize_snapshot_state(
        &mut state,
        entity_type,
        entity_id,
        table,
        sequence_nr,
    )?;
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
/// a journal-provenance snapshot is the exact replay baseline and only its
/// unsnapshotted tail is applied. Legacy or terminal-hiding snapshots use the
/// same strict full-history migration path as ordinary actor recovery. Both
/// sources are re-read byte-for-byte/high-water-for-high-water before return.
pub(crate) async fn recover_entity_state_from_stable_sources(
    context: EntityRecoveryContext<'_>,
) -> Result<StableEntitySource, ActorError> {
    let persistence_id = format!(
        "{}:{}:{}",
        context.tenant, context.entity_type, context.entity_id
    );
    for _attempt in 1..=MAX_STABLE_SOURCE_ATTEMPTS {
        let initial_boundary = context
            .store
            .journal_boundary(&persistence_id)
            .await
            .map_err(|error| {
                ActorError::custom(format!(
                    "failed to read durable journal source for {}:{}: {error}",
                    context.entity_type, context.entity_id
                ))
            })?;
        let loaded_snapshot = load_snapshot_strict(
            context.entity_type,
            context.entity_id,
            context.table,
            context.store,
            context.initial_fields,
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
                replayed_state_materialization: false,
            }
        } else {
            let captured_source = CapturedReplaySource {
                journal_boundary: initial_boundary,
                snapshot: snapshot.clone(),
            };
            let mut state = EntityActor::build_initial_state(
                context.entity_type,
                context.entity_id,
                context.table,
                context.initial_fields,
            );
            let replay = replay_events(
                context,
                &mut state,
                ReplayPolicy {
                    strict_journal_read: true,
                    load_snapshot: true,
                    strict_event_decode: true,
                    replay_full_journal: false,
                },
                Some(&captured_source),
            )
            .await?;
            StableEntitySource {
                journal_sequence: state.sequence_nr,
                state: Some(state),
                snapshot,
                replayed_state_materialization: replay.replayed_state_materialization,
            }
        };
        if stable_entity_source_is_current(context.store, &persistence_id, &source).await? {
            return Ok(source);
        }
    }
    Err(ActorError::custom(format!(
        "durable source generation for {}:{} did not stabilize after {MAX_STABLE_SOURCE_ATTEMPTS} attempts",
        context.entity_type, context.entity_id
    )))
}
