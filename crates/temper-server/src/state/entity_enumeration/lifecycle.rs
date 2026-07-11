//! Lifecycle-aware classification of raw journal tails.

use temper_runtime::persistence::{COMPOSITE_EVENT_TYPE, PersistenceEnvelope, PersistenceError};

use crate::storage::BoxedEventStore;

const LIFECYCLE_LOOKBACK_BUDGET: usize = 1_024;

#[derive(Clone)]
pub(in crate::state) struct LatestEntityLifecycle {
    pub(in crate::state) lifecycle_event: PersistenceEnvelope,
    pub(in crate::state) raw_sequence: u64,
}

pub(in crate::state) async fn read_latest_entity_lifecycle(
    store: &BoxedEventStore,
    persistence_id: &str,
) -> Result<Option<LatestEntityLifecycle>, PersistenceError> {
    let mut events = read_latest_entity_lifecycles(store, &[persistence_id.to_string()]).await?;
    if events.len() != 1 {
        return Err(PersistenceError::Storage(format!(
            "latest-event read returned {} rows for one stream",
            events.len()
        )));
    }
    Ok(events.pop().flatten())
}

pub(in crate::state) async fn read_latest_entity_lifecycles(
    store: &BoxedEventStore,
    persistence_ids: &[String],
) -> Result<Vec<Option<LatestEntityLifecycle>>, PersistenceError> {
    let raw_latest = store.read_latest_events(persistence_ids).await?;
    if raw_latest.len() != persistence_ids.len() {
        return Err(PersistenceError::Storage(format!(
            "latest-event read returned {} rows for {} streams",
            raw_latest.len(),
            persistence_ids.len()
        )));
    }
    let mut classified = Vec::with_capacity(raw_latest.len());
    for (persistence_id, raw_event) in persistence_ids.iter().zip(raw_latest) {
        let Some(raw_event) = raw_event else {
            classified.push(None);
            continue;
        };
        let raw_sequence = raw_event.sequence_nr;
        let lifecycle_event = if raw_event.event_type == COMPOSITE_EVENT_TYPE {
            latest_lifecycle_before_audit_tail(store, persistence_id, raw_sequence).await?
        } else {
            raw_event
        };
        classified.push(Some(LatestEntityLifecycle {
            lifecycle_event,
            raw_sequence,
        }));
    }
    Ok(classified)
}

async fn latest_lifecycle_before_audit_tail(
    store: &BoxedEventStore,
    persistence_id: &str,
    raw_sequence: u64,
) -> Result<PersistenceEnvelope, PersistenceError> {
    let from_sequence = raw_sequence.saturating_sub(LIFECYCLE_LOOKBACK_BUDGET as u64);
    let read_limit = LIFECYCLE_LOOKBACK_BUDGET
        .checked_add(1)
        .ok_or_else(|| PersistenceError::Storage("lifecycle read budget overflowed".to_string()))?;
    let events = store
        .read_events_bounded(persistence_id, from_sequence, read_limit)
        .await?;
    validate_lifecycle_lookback(persistence_id, from_sequence, raw_sequence, &events)?;
    events
        .into_iter()
        .rev()
        .find(|event| event.event_type != COMPOSITE_EVENT_TYPE)
        .ok_or_else(|| {
            PersistenceError::Storage(format!(
                "no lifecycle event found within bounded audit lookback for {persistence_id}"
            ))
        })
}

fn validate_lifecycle_lookback(
    persistence_id: &str,
    from_sequence: u64,
    raw_sequence: u64,
    events: &[PersistenceEnvelope],
) -> Result<(), PersistenceError> {
    if events.len() > LIFECYCLE_LOOKBACK_BUDGET {
        return Err(PersistenceError::Storage(format!(
            "lifecycle lookback budget exceeded for {persistence_id}"
        )));
    }
    let mut expected = from_sequence;
    for event in events {
        expected = expected.checked_add(1).ok_or_else(|| {
            PersistenceError::Storage(format!(
                "lifecycle lookback sequence overflow for {persistence_id}"
            ))
        })?;
        if event.sequence_nr != expected {
            return Err(PersistenceError::Storage(format!(
                "non-contiguous lifecycle lookback for {persistence_id}: expected sequence {expected}, found {}",
                event.sequence_nr
            )));
        }
    }
    if expected != raw_sequence {
        return Err(PersistenceError::Storage(format!(
            "incomplete lifecycle lookback for {persistence_id}: recovered through {expected}, expected raw tail {raw_sequence}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use temper_runtime::persistence::{
        EventMetadata, EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope,
        PersistenceError,
    };
    use temper_runtime::scheduler::{sim_now, sim_uuid};

    use super::{
        LIFECYCLE_LOOKBACK_BUDGET, read_latest_entity_lifecycle, validate_lifecycle_lookback,
    };
    use crate::storage::BoxedEventStore;

    fn envelope(sequence_nr: u64) -> PersistenceEnvelope {
        PersistenceEnvelope {
            sequence_nr,
            event_type: "CompositeEvent".to_string(),
            payload: serde_json::json!({}),
            metadata: EventMetadata {
                event_id: sim_uuid(),
                causation_id: sim_uuid(),
                correlation_id: sim_uuid(),
                timestamp: sim_now(),
                actor_id: "lifecycle-lookback-test".to_string(),
            },
        }
    }

    #[test]
    fn lifecycle_lookback_rejects_an_internal_sequence_gap() {
        let events = [envelope(1), envelope(3)];
        let error = validate_lifecycle_lookback("tenant:Order:gap", 0, 3, &events)
            .expect_err("a successful but truncated read must fail closed");
        assert!(error.to_string().contains("expected sequence 2, found 3"));
    }

    #[derive(Clone)]
    struct LaterTailStore;

    impl EventStore for LaterTailStore {
        async fn append(
            &self,
            _persistence_id: &str,
            _expected_sequence: u64,
            _events: &[PersistenceEnvelope],
        ) -> Result<u64, PersistenceError> {
            unreachable!("append is not used by the lifecycle lookback test")
        }

        async fn append_batch(
            &self,
            _appends: &[PersistenceAppend],
        ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
            unreachable!("append_batch is not used by the lifecycle lookback test")
        }

        async fn read_events(
            &self,
            _persistence_id: &str,
            _from_sequence: u64,
        ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
            panic!("lifecycle lookback must not use an unbounded journal read")
        }

        async fn read_events_bounded(
            &self,
            _persistence_id: &str,
            _from_sequence: u64,
            limit: usize,
        ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
            assert_eq!(limit, LIFECYCLE_LOOKBACK_BUDGET + 1);
            Ok((1..=limit)
                .map(|sequence| envelope(sequence as u64))
                .collect())
        }

        async fn read_latest_events(
            &self,
            persistence_ids: &[String],
        ) -> Result<Vec<Option<PersistenceEnvelope>>, PersistenceError> {
            assert_eq!(persistence_ids, &["default:Order:racing-tail"]);
            Ok(vec![Some(envelope(LIFECYCLE_LOOKBACK_BUDGET as u64))])
        }

        async fn save_snapshot(
            &self,
            _persistence_id: &str,
            _sequence_nr: u64,
            _snapshot: &[u8],
        ) -> Result<(), PersistenceError> {
            unreachable!("save_snapshot is not used by the lifecycle lookback test")
        }

        async fn load_snapshot(
            &self,
            _persistence_id: &str,
        ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
            unreachable!("load_snapshot is not used by the lifecycle lookback test")
        }

        async fn list_entity_ids(
            &self,
            _tenant: &str,
        ) -> Result<Vec<(String, String)>, PersistenceError> {
            unreachable!("list_entity_ids is not used by the lifecycle lookback test")
        }

        async fn list_entity_ids_by_type(
            &self,
            _tenant: &str,
            _entity_type: &str,
        ) -> Result<Vec<String>, PersistenceError> {
            unreachable!("list_entity_ids_by_type is not used by the lifecycle lookback test")
        }
    }

    #[tokio::test]
    async fn lifecycle_lookback_is_storage_bounded_when_tail_advances() {
        let store = BoxedEventStore::new(LaterTailStore);
        let Err(error) = read_latest_entity_lifecycle(&store, "default:Order:racing-tail").await
        else {
            panic!("an event after the captured tail must fail closed");
        };
        assert!(error.to_string().contains("lookback budget exceeded"));
    }
}
