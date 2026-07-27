//! Shared append-batch lifecycle and structural validation.

use super::{
    COMPOSITE_EVENT_TYPE, PersistenceAppend, PersistenceEnvelope, PersistenceError,
    PersistenceSequenceGuard, is_deletion_tombstone,
};

/// Return whether an append contains any entity-deletion tombstone.
///
/// A raw append that crosses a deletion boundary cannot safely preserve key
/// claims from the prior entity generation, even if a later event recreates it.
pub fn contains_deletion_tombstone(events: &[PersistenceEnvelope]) -> bool {
    events.iter().any(is_deletion_tombstone)
}

/// Return whether the appended lifecycle ends in deletion.
///
/// `CompositeEvent` records are audit envelopes, so a tombstone followed only
/// by those records remains deleted. A later non-audit lifecycle event becomes
/// the new tail and determines the result.
pub fn ends_in_deletion_tombstone(events: &[PersistenceEnvelope]) -> bool {
    events
        .iter()
        .rev()
        .find(|event| event.event_type != COMPOSITE_EVENT_TYPE)
        .is_some_and(is_deletion_tombstone)
}

/// Reject batches whose IDs resolve to the same physical persistence stream.
///
/// Backends share this check so legacy/default-tenant aliases and identical
/// empty or non-empty members have one contract before any transaction begins.
pub fn validate_persistence_append_batch(
    appends: &[PersistenceAppend],
) -> Result<(), PersistenceError> {
    let mut seen = std::collections::BTreeSet::new();
    for append in appends {
        let stream_key = crate::tenant::parse_persistence_id_parts(&append.persistence_id)
            .map_err(PersistenceError::Storage)?;
        if !seen.insert(stream_key) {
            return Err(PersistenceError::Storage(format!(
                "duplicate persistence_id '{}' resolves to stream '{}:{}:{}' in append_batch",
                append.persistence_id, stream_key.0, stream_key.1, stream_key.2
            )));
        }
    }
    Ok(())
}

/// Validate one atomic append plus its compare-only journal guards.
///
/// A guard names a stream whose sequence must still match when the append
/// commits. Guard and append members must be physically distinct: accepting a
/// duplicate or a legacy/default-tenant alias would make it ambiguous whether
/// the stream is a write target or only a precondition.
pub fn validate_guarded_persistence_append_batch(
    appends: &[PersistenceAppend],
    guards: &[PersistenceSequenceGuard],
) -> Result<(), PersistenceError> {
    validate_persistence_append_batch(appends)?;
    if guards.is_empty() {
        return Ok(());
    }
    if !appends.iter().any(|append| !append.events.is_empty()) {
        return Err(PersistenceError::Storage(
            "guarded append requires at least one durable event".to_string(),
        ));
    }

    let mut seen = std::collections::BTreeSet::new();
    for append in appends {
        let stream_key = crate::tenant::parse_persistence_id_parts(&append.persistence_id)
            .map_err(PersistenceError::Storage)?;
        seen.insert(stream_key);
    }
    for guard in guards {
        let stream_key = crate::tenant::parse_persistence_id_parts(&guard.persistence_id)
            .map_err(PersistenceError::Storage)?;
        if !seen.insert(stream_key) {
            return Err(PersistenceError::Storage(format!(
                "guard persistence_id '{}' duplicates an append or guard stream",
                guard.persistence_id
            )));
        }
    }
    Ok(())
}
