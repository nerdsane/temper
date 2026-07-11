//! Deterministic event-segment bookkeeping shared by append paths.

use std::collections::BTreeMap;

use temper_runtime::persistence::PersistenceError;

/// Observable segment metadata for deterministic store assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimEventSegment {
    /// Monotonic segment number within one stream.
    pub segment_index: u64,
    /// First journal sequence assigned to this segment.
    pub start_sequence_nr: u64,
    /// Last journal sequence currently assigned to this segment.
    pub end_sequence_nr: Option<u64>,
    /// Snapshot boundary that sealed this segment.
    pub snapshot_sequence: Option<u64>,
    /// Number of journal events assigned to this segment.
    pub event_count: u64,
    /// Whether no later append may use this segment.
    pub sealed: bool,
}

pub(crate) fn record_segment_append(
    event_segments: &mut BTreeMap<String, Vec<SimEventSegment>>,
    persistence_id: &str,
    expected_sequence: u64,
    new_sequence: u64,
) -> Result<(), PersistenceError> {
    if new_sequence == expected_sequence {
        return Ok(());
    }
    let start_sequence = expected_sequence.checked_add(1).ok_or_else(|| {
        PersistenceError::Storage("event sequence exhausted while opening segment".to_string())
    })?;
    let segments = event_segments
        .entry(persistence_id.to_string())
        .or_insert_with(|| {
            vec![SimEventSegment {
                segment_index: 0,
                start_sequence_nr: start_sequence,
                end_sequence_nr: None,
                snapshot_sequence: None,
                event_count: 0,
                sealed: false,
            }]
        });
    if segments.iter().filter(|segment| !segment.sealed).count() > 1 {
        return Err(PersistenceError::Storage(format!(
            "multiple open event segments for {persistence_id}"
        )));
    }
    if segments.last().is_none_or(|segment| segment.sealed) {
        let next_index = match segments.last() {
            Some(segment) => segment.segment_index.checked_add(1).ok_or_else(|| {
                PersistenceError::Storage("event segment index exhausted".to_string())
            })?,
            None => 0,
        };
        segments.push(SimEventSegment {
            segment_index: next_index,
            start_sequence_nr: start_sequence,
            end_sequence_nr: None,
            snapshot_sequence: None,
            event_count: 0,
            sealed: false,
        });
    }
    let active = segments
        .last_mut()
        .expect("segments must contain an active segment");
    if active.sealed || active.start_sequence_nr > start_sequence {
        return Err(PersistenceError::Storage(format!(
            "invalid active event segment for {persistence_id}"
        )));
    }
    active.end_sequence_nr = Some(new_sequence);
    active.event_count = new_sequence
        .checked_sub(active.start_sequence_nr)
        .and_then(|count| count.checked_add(1))
        .ok_or_else(|| {
            PersistenceError::Storage(format!("invalid event segment range for {persistence_id}"))
        })?;
    Ok(())
}

pub(crate) fn rotate_for_snapshot(
    event_segments: &BTreeMap<String, Vec<SimEventSegment>>,
    persistence_id: &str,
    journal_tail: u64,
    snapshot_sequence: u64,
) -> Result<Option<Vec<SimEventSegment>>, PersistenceError> {
    if snapshot_sequence == 0 || snapshot_sequence != journal_tail {
        return Ok(None);
    }
    let mut segments = event_segments.get(persistence_id).cloned().ok_or_else(|| {
        PersistenceError::Storage(format!(
            "journal tail {snapshot_sequence} has no event segment for {persistence_id}"
        ))
    })?;
    if segments
        .iter()
        .any(|segment| segment.snapshot_sequence == Some(snapshot_sequence))
    {
        return Ok(None);
    }
    let active = segments.last_mut().ok_or_else(|| {
        PersistenceError::Storage(format!("event segments are empty for {persistence_id}"))
    })?;
    if active.sealed || active.end_sequence_nr != Some(snapshot_sequence) || active.event_count == 0
    {
        return Err(PersistenceError::Storage(format!(
            "journal tail {snapshot_sequence} does not match the active event segment for {persistence_id}"
        )));
    }
    active.snapshot_sequence = Some(snapshot_sequence);
    active.sealed = true;
    let next_index = active
        .segment_index
        .checked_add(1)
        .ok_or_else(|| PersistenceError::Storage("event segment index exhausted".to_string()))?;
    let next_sequence = snapshot_sequence.checked_add(1).ok_or_else(|| {
        PersistenceError::Storage("event sequence exhausted after snapshot".to_string())
    })?;
    segments.push(SimEventSegment {
        segment_index: next_index,
        start_sequence_nr: next_sequence,
        end_sequence_nr: None,
        snapshot_sequence: None,
        event_count: 0,
        sealed: false,
    });
    Ok(Some(segments))
}
