//! Strict journal replay validation.

use temper_jit::TransitionTable;
use temper_runtime::actor::ActorError;
use temper_runtime::persistence::PersistenceEnvelope;

use crate::storage::BoxedEventStore;

use super::EntityEvent;

pub(super) async fn validate_strict_replay(
    store: &BoxedEventStore,
    persistence_id: &str,
    from_sequence: u64,
    envelopes: &[PersistenceEnvelope],
) -> Result<(), ActorError> {
    let expected = validate_contiguous_sequences(persistence_id, from_sequence, envelopes)?;

    let mut latest = store
        .read_latest_events(&[persistence_id.to_string()])
        .await
        .map_err(|error| {
            ActorError::custom(format!(
                "failed to read journal tail for strict replay of {persistence_id}: {error}"
            ))
        })?;
    if latest.len() != 1 {
        return Err(ActorError::custom(format!(
            "journal tail read for strict replay of {persistence_id} returned {} rows, expected one",
            latest.len()
        )));
    }
    let durable_sequence = latest.pop().flatten().map_or(0, |event| event.sequence_nr);
    if durable_sequence != expected {
        return Err(ActorError::custom(format!(
            "incomplete journal for strict replay of {persistence_id}: recovered through sequence {expected}, durable tail is {durable_sequence}"
        )));
    }
    Ok(())
}

fn validate_contiguous_sequences(
    persistence_id: &str,
    from_sequence: u64,
    envelopes: &[PersistenceEnvelope],
) -> Result<u64, ActorError> {
    let mut expected = from_sequence;
    for envelope in envelopes {
        expected = expected.checked_add(1).ok_or_else(|| {
            ActorError::custom(format!(
                "journal sequence overflow during strict replay of {persistence_id}"
            ))
        })?;
        if envelope.sequence_nr != expected {
            return Err(ActorError::custom(format!(
                "non-contiguous journal for strict replay of {persistence_id}: expected sequence {expected}, found {}",
                envelope.sequence_nr
            )));
        }
    }
    Ok(expected)
}

pub(super) fn validate_recreation_event(
    table: &TransitionTable,
    envelope: &PersistenceEnvelope,
    event: &EntityEvent,
    entity_type: &str,
    entity_id: &str,
) -> Result<(), ActorError> {
    let valid = envelope.event_type == "Created"
        && event.action == "Created"
        && event.from_status.is_empty()
        && event.to_status == table.initial_state
        && event.params.is_object();
    if valid {
        return Ok(());
    }
    Err(ActorError::custom(format!(
        "invalid recreation event for {entity_type}:{entity_id} at sequence {}: event_type='{}', action='{}', from_status='{}', to_status='{}', expected initial status '{}'",
        envelope.sequence_nr,
        envelope.event_type,
        event.action,
        event.from_status,
        event.to_status,
        table.initial_state
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
    use temper_runtime::scheduler::{sim_now, sim_uuid};
    use temper_store_sim::{SimEventStore, SimFaultConfig};

    fn envelope(sequence_nr: u64) -> PersistenceEnvelope {
        PersistenceEnvelope {
            sequence_nr,
            event_type: "Created".to_string(),
            payload: serde_json::json!({}),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: "strict-replay-test".to_string(),
            },
        }
    }

    #[test]
    fn contiguous_sequence_check_rejects_internal_gap() {
        let envelopes = [envelope(4), envelope(6)];
        let error = validate_contiguous_sequences("tenant:Order:gap", 3, &envelopes)
            .expect_err("gap must be rejected");
        assert!(error.to_string().contains("expected sequence 5, found 6"));
    }

    #[tokio::test]
    async fn strict_validation_rejects_successful_truncated_read() {
        let store = SimEventStore::no_faults(192_500);
        let persistence_id = "tenant:Order:truncated";
        store
            .append(persistence_id, 0, &[envelope(0), envelope(0)])
            .await
            .unwrap();
        store.restore_faults(SimFaultConfig {
            read_truncation_prob: 1.0,
            ..SimFaultConfig::none()
        });
        let truncated = store.read_events(persistence_id, 0).await.unwrap();
        assert_eq!(truncated.len(), 1, "fault must return a successful prefix");
        let boxed = BoxedEventStore::new(store);
        let error = validate_strict_replay(&boxed, persistence_id, 0, &truncated)
            .await
            .expect_err("strict replay must compare against the durable tail");
        assert!(error.to_string().contains("durable tail is 2"));
    }
}
