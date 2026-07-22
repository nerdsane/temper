//! Snapshot-source fences and segment materialization.

use super::*;

pub(super) fn snapshot_source_matches(
    current: Option<&(u64, Vec<u8>)>,
    expected: &SnapshotSourceFence,
) -> bool {
    match expected {
        SnapshotSourceFence::Unchecked => true,
        SnapshotSourceFence::Absent => current.is_none(),
        SnapshotSourceFence::Exact { sequence_nr, state } => current.is_some_and(|current| {
            current.0 == *sequence_nr && current.1.as_slice() == state.as_slice()
        }),
    }
}

pub(super) fn append_retires_snapshot(
    expected_sequence: u64,
    events: &[PersistenceEnvelope],
    snapshot_source: &SnapshotSourceFence,
    entity_type: &str,
    entity_id: &str,
) -> bool {
    expected_sequence == 0
        && matches!(snapshot_source, SnapshotSourceFence::Exact { .. })
        && events
            .first()
            .is_some_and(|event| is_state_materialization_event_for(event, entity_type, entity_id))
}

pub(super) fn update_segments_after_append_locked(
    inner: &mut SimEventStoreInner,
    persistence_id: &str,
    expected_sequence: u64,
    new_sequence: u64,
) {
    if new_sequence == expected_sequence {
        return;
    }
    let segments = inner
        .event_segments
        .entry(persistence_id.to_string())
        .or_default();
    let invalid_open = segments.last().is_some_and(|segment| {
        !segment.sealed && segment.start_sequence_nr > expected_sequence.saturating_add(1)
    });
    if expected_sequence == 0 || invalid_open {
        // Snapshot-only generations have no journal segment. Reset legacy
        // metadata that extends beyond the journal before assigning events.
        segments.clear();
    }
    if segments.is_empty() && expected_sequence > 0 {
        segments.push(SimEventSegment {
            segment_index: 0,
            start_sequence_nr: 1,
            end_sequence_nr: Some(expected_sequence),
            snapshot_sequence: None,
            event_count: expected_sequence,
            sealed: false,
        });
    }
    if segments
        .last()
        .map(|segment| segment.sealed)
        .unwrap_or(true)
    {
        let next_index = segments
            .last()
            .map(|segment| segment.segment_index + 1)
            .unwrap_or(0);
        segments.push(SimEventSegment {
            segment_index: next_index,
            start_sequence_nr: expected_sequence.saturating_add(1).max(1),
            end_sequence_nr: None,
            snapshot_sequence: None,
            event_count: 0,
            sealed: false,
        });
    }
    let active = segments
        .last_mut()
        .expect("a non-empty append must have an active segment");
    active.end_sequence_nr = Some(new_sequence);
    active.event_count = new_sequence
        .saturating_sub(active.start_sequence_nr)
        .saturating_add(1);
}

impl SimEventStore {
    pub(super) fn save_snapshot_locked(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
        source: &SnapshotSourceFence,
        key_contract: Option<&str>,
    ) -> Result<(), PersistenceError> {
        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        if !snapshot_source_matches(inner.snapshots.get(persistence_id), source) {
            return Err(PersistenceError::SnapshotGenerationChanged);
        }
        let journal = inner.journals.get(persistence_id);
        let journal_high_water = journal
            .and_then(|events| events.last())
            .map(|event| event.sequence_nr)
            .unwrap_or(0);
        if matches!(source, SnapshotSourceFence::Unchecked)
            && journal_high_water > 0
            && journal
                .and_then(|events| events.first())
                .is_some_and(|event| {
                    is_state_materialization_event_for(event, entity_type, entity_id)
                })
        {
            // The first journal append already consumed the migration snapshot.
            // A delayed unchecked writer belongs to the retired snapshot-only
            // generation regardless of whether the journal has since caught up
            // with its numeric sequence.
            return Ok(());
        }
        let current_snapshot = inner.snapshots.get(persistence_id);
        let snapshot_is_noop = current_snapshot.is_some_and(|(stored_sequence, stored)| {
            *stored_sequence > sequence_nr
                || (*stored_sequence == sequence_nr && stored.as_slice() == snapshot)
        });
        if snapshot_is_noop {
            return Ok(());
        }
        let same_sequence_replacement =
            current_snapshot.is_some_and(|(stored_sequence, _)| *stored_sequence == sequence_nr);

        // Fault injection: snapshot save failure.
        let sf_prob = inner.faults.snapshot_failure_prob;
        if inner.rng.chance(sf_prob) {
            return Err(PersistenceError::Storage(
                "SimEventStore: injected snapshot failure".into(),
            ));
        }

        if let Some(key_contract) = key_contract {
            // Source mismatches and injected failures return before touching
            // the contract. From this point onward all updates are infallible
            // map mutations, mirroring one transactional store commit.
            reconcile_key_contract_locked(
                &mut inner,
                tenant,
                entity_type,
                Some(key_contract),
                KeyContractUse::LiveWrite,
            )?;
        } else {
            invalidate_coverage_for_snapshot_write_locked(&mut inner, persistence_id)?;
        }

        inner
            .snapshots
            .insert(persistence_id.to_string(), (sequence_nr, snapshot.to_vec()));
        inner
            .snapshot_history
            .entry(persistence_id.to_string())
            .or_default()
            .insert(sequence_nr, snapshot.to_vec());
        if same_sequence_replacement {
            return Ok(());
        }
        if journal_high_water == 0 {
            inner.event_segments.remove(persistence_id);
            return Ok(());
        }
        if sequence_nr > journal_high_water {
            // Snapshots accelerate recovery but do not invent journal history.
            // A migration baseline ahead of the journal is not an event boundary.
            return Ok(());
        }
        let segments = inner
            .event_segments
            .entry(persistence_id.to_string())
            .or_insert_with(|| {
                vec![SimEventSegment {
                    segment_index: 0,
                    start_sequence_nr: 1,
                    end_sequence_nr: (journal_high_water > 0).then_some(journal_high_water),
                    snapshot_sequence: None,
                    event_count: journal_high_water,
                    sealed: false,
                }]
            });
        if segments.last().map(|s| s.sealed).unwrap_or(true) {
            let idx = segments.last().map(|s| s.segment_index + 1).unwrap_or(0);
            segments.push(SimEventSegment {
                segment_index: idx,
                start_sequence_nr: 1,
                end_sequence_nr: (journal_high_water > 0).then_some(journal_high_water),
                snapshot_sequence: None,
                event_count: journal_high_water,
                sealed: false,
            });
        }
        let active = segments
            .last_mut()
            .expect("segments must contain an active segment");
        active.end_sequence_nr = Some(sequence_nr);
        active.snapshot_sequence = Some(sequence_nr);
        active.event_count = sequence_nr
            .saturating_sub(active.start_sequence_nr)
            .saturating_add(1);
        active.sealed = true;
        let next_index = active.segment_index + 1;
        let tail_end = (journal_high_water > sequence_nr).then_some(journal_high_water);
        segments.push(SimEventSegment {
            segment_index: next_index,
            start_sequence_nr: sequence_nr + 1,
            end_sequence_nr: tail_end,
            snapshot_sequence: None,
            event_count: journal_high_water.saturating_sub(sequence_nr),
            sealed: false,
        });
        Ok(())
    }
}
