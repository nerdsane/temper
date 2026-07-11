//! Recovery of writes that committed before their acknowledgement was lost.

use temper_runtime::persistence::{
    LATEST_EVENT_BATCH_SIZE, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope,
    PersistenceError, validate_persistence_append_batch,
};

use super::DynEventStore;

#[derive(Clone, Copy)]
struct AppendProof<'a> {
    persistence_id: &'a str,
    expected_sequence: u64,
    events: &'a [PersistenceEnvelope],
}

pub(super) async fn reconcile_append_result(
    store: &dyn DynEventStore,
    persistence_id: &str,
    expected_sequence: u64,
    events: &[PersistenceEnvelope],
    result: Result<u64, PersistenceError>,
) -> Result<u64, PersistenceError> {
    let Err(error) = result else {
        return result;
    };
    if events.is_empty() {
        return Err(error);
    }
    let read_limit = reconciliation_read_limit(events.len())?;
    let persisted = match store
        .read_events_bounded(persistence_id, expected_sequence, read_limit)
        .await
    {
        Ok(persisted) => persisted,
        Err(_) => return Err(error),
    };
    let proof = [AppendProof {
        persistence_id,
        expected_sequence,
        events,
    }];
    if !append_events_match(expected_sequence, events, &persisted)
        || !latest_tails_match(store, &proof).await.unwrap_or(false)
    {
        return Err(error);
    }
    let sequence_nr = sequence_after(expected_sequence, events.len())?;
    tracing::warn!(
        persistence_id,
        sequence_nr,
        original_error = %error,
        "reconciled ambiguous append acknowledgement from durable event ids"
    );
    Ok(sequence_nr)
}

pub(super) async fn reconcile_batch_result(
    store: &dyn DynEventStore,
    appends: &[PersistenceAppend],
    result: Result<Vec<PersistenceAppendResult>, PersistenceError>,
) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
    validate_persistence_append_batch(appends)?;
    let Err(error) = result else {
        return result;
    };
    // `Some(rows)` is a complete declared-key replacement, including an
    // explicit clear. The journal does not record that replacement intent, so
    // an event-only proof cannot distinguish K1 from K2 or None from Some([]).
    if appends.iter().any(|append| append.key_rows.is_some()) {
        return Err(error);
    }
    if appends.iter().all(|append| append.events.is_empty()) {
        return Err(error);
    }
    let mut reconciled = Vec::with_capacity(appends.len());
    for append in appends {
        if !append.events.is_empty() {
            let read_limit = reconciliation_read_limit(append.events.len())?;
            let persisted = match store
                .read_events_bounded(&append.persistence_id, append.expected_sequence, read_limit)
                .await
            {
                Ok(persisted) => persisted,
                Err(_) => return Err(error),
            };
            if !append_events_match(append.expected_sequence, &append.events, &persisted) {
                return Err(error);
            }
        }
        reconciled.push(PersistenceAppendResult {
            persistence_id: append.persistence_id.clone(),
            sequence_nr: sequence_after(append.expected_sequence, append.events.len())?,
        });
    }
    let proofs = appends
        .iter()
        .filter(|append| !append.events.is_empty())
        .map(|append| AppendProof {
            persistence_id: &append.persistence_id,
            expected_sequence: append.expected_sequence,
            events: &append.events,
        })
        .collect::<Vec<_>>();
    if !latest_tails_match(store, &proofs).await.unwrap_or(false) {
        return Err(error);
    }
    tracing::warn!(
        streams = reconciled.len(),
        original_error = %error,
        "reconciled ambiguous batch acknowledgement from durable event ids"
    );
    Ok(reconciled)
}

async fn latest_tails_match(
    store: &dyn DynEventStore,
    proofs: &[AppendProof<'_>],
) -> Result<bool, PersistenceError> {
    for chunk in proofs.chunks(LATEST_EVENT_BATCH_SIZE) {
        let persistence_ids = chunk
            .iter()
            .map(|proof| proof.persistence_id.to_string())
            .collect::<Vec<_>>();
        let latest = store.read_latest_events(&persistence_ids).await?;
        if latest.len() != chunk.len() {
            return Ok(false);
        }
        for (proof, latest) in chunk.iter().zip(latest) {
            let sequence_nr = sequence_after(proof.expected_sequence, proof.events.len())?;
            let Some(attempted_last) = proof.events.last() else {
                return Ok(false);
            };
            if !latest.as_ref().is_some_and(|latest| {
                persistence_envelopes_match(sequence_nr, attempted_last, latest)
            }) {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

fn append_events_match(
    expected_sequence: u64,
    attempted: &[PersistenceEnvelope],
    persisted: &[PersistenceEnvelope],
) -> bool {
    if persisted.len() != attempted.len() {
        return false;
    }
    attempted
        .iter()
        .zip(persisted)
        .enumerate()
        .all(|(index, (attempted, persisted))| {
            let Ok(index) = u64::try_from(index) else {
                return false;
            };
            let Some(sequence_nr) = expected_sequence
                .checked_add(index)
                .and_then(|sequence| sequence.checked_add(1))
            else {
                return false;
            };
            persistence_envelopes_match(sequence_nr, attempted, persisted)
        })
}

fn persistence_envelopes_match(
    sequence_nr: u64,
    attempted: &PersistenceEnvelope,
    persisted: &PersistenceEnvelope,
) -> bool {
    persisted.sequence_nr == sequence_nr
        && persisted.metadata.event_id == attempted.metadata.event_id
        && persisted.metadata.causation_id == attempted.metadata.causation_id
        && persisted.metadata.correlation_id == attempted.metadata.correlation_id
        && persisted.metadata.timestamp == attempted.metadata.timestamp
        && persisted.metadata.actor_id == attempted.metadata.actor_id
        && persisted.event_type == attempted.event_type
        && persisted.payload == attempted.payload
}

fn reconciliation_read_limit(event_count: usize) -> Result<usize, PersistenceError> {
    event_count.checked_add(1).ok_or_else(|| {
        PersistenceError::Storage("append reconciliation read budget overflowed".to_string())
    })
}

fn sequence_after(expected_sequence: u64, event_count: usize) -> Result<u64, PersistenceError> {
    let event_count = u64::try_from(event_count).map_err(|_| {
        PersistenceError::Storage("append event count exceeds sequence range".to_string())
    })?;
    expected_sequence.checked_add(event_count).ok_or_else(|| {
        PersistenceError::Storage("append sequence exceeds persistence range".to_string())
    })
}

#[cfg(test)]
mod tests {
    use temper_runtime::persistence::EventMetadata;

    use super::*;

    fn envelope(sequence_nr: u64) -> PersistenceEnvelope {
        PersistenceEnvelope {
            sequence_nr,
            event_type: "Created".to_string(),
            payload: serde_json::json!({"id": "entity-1"}),
            metadata: EventMetadata {
                event_id: uuid::Uuid::from_u128(1),
                causation_id: uuid::Uuid::from_u128(2),
                correlation_id: uuid::Uuid::from_u128(3),
                timestamp: chrono::DateTime::UNIX_EPOCH,
                actor_id: "actor-1".to_string(),
            },
        }
    }

    #[test]
    fn reconciliation_match_requires_the_exact_tail_and_metadata() {
        let attempted = envelope(0);
        let persisted = envelope(1);
        assert!(append_events_match(
            0,
            std::slice::from_ref(&attempted),
            std::slice::from_ref(&persisted)
        ));

        let mut later = envelope(2);
        later.metadata.event_id = uuid::Uuid::from_u128(4);
        let persisted_with_later = [persisted.clone(), later];
        assert!(!append_events_match(
            0,
            std::slice::from_ref(&attempted),
            &persisted_with_later
        ));

        let mut altered_metadata = persisted;
        altered_metadata.metadata.actor_id = "different-actor".to_string();
        assert!(!append_events_match(0, &[attempted], &[altered_metadata]));
    }
}
