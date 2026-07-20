//! Deterministic simulation event-store snapshots operations.

use super::*;

impl SimEventStore {
    pub(super) async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let snapshot_delay = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let delay = inner
                .pending_snapshot_delays
                .get_mut(persistence_id)
                .and_then(VecDeque::pop_front);
            if inner
                .pending_snapshot_delays
                .get(persistence_id)
                .is_some_and(VecDeque::is_empty)
            {
                inner.pending_snapshot_delays.remove(persistence_id);
            }
            delay
        };
        if let Some(delay) = snapshot_delay
            && !delay.is_zero()
        {
            tokio::time::sleep(delay).await;
        }

        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        // Fault injection: snapshot save failure.
        let sf_prob = inner.faults.snapshot_failure_prob;
        if inner.rng.chance(sf_prob) {
            return Err(PersistenceError::Storage(
                "SimEventStore: injected snapshot failure".into(),
            ));
        }

        inner
            .snapshots
            .insert(persistence_id.to_string(), (sequence_nr, snapshot.to_vec()));
        inner
            .snapshot_history
            .entry(persistence_id.to_string())
            .or_default()
            .insert(sequence_nr, snapshot.to_vec());
        let segments = inner
            .event_segments
            .entry(persistence_id.to_string())
            .or_insert_with(|| {
                vec![SimEventSegment {
                    segment_index: 0,
                    start_sequence_nr: 1,
                    end_sequence_nr: Some(sequence_nr),
                    snapshot_sequence: None,
                    event_count: sequence_nr,
                    sealed: false,
                }]
            });
        if segments.last().map(|s| s.sealed).unwrap_or(true) {
            let idx = segments.last().map(|s| s.segment_index + 1).unwrap_or(0);
            segments.push(SimEventSegment {
                segment_index: idx,
                start_sequence_nr: 1,
                end_sequence_nr: Some(sequence_nr),
                snapshot_sequence: None,
                event_count: sequence_nr,
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
        segments.push(SimEventSegment {
            segment_index: next_index,
            start_sequence_nr: sequence_nr + 1,
            end_sequence_nr: None,
            snapshot_sequence: None,
            event_count: 0,
            sealed: false,
        });
        Ok(())
    }

    pub(super) async fn replace_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        expected_snapshot: &[u8],
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        let sf_prob = inner.faults.snapshot_failure_prob;
        if inner.rng.chance(sf_prob) {
            return Err(PersistenceError::Storage(
                "SimEventStore: injected snapshot failure".into(),
            ));
        }

        let Some((actual_sequence, actual_snapshot)) = inner.snapshots.get(persistence_id) else {
            return Err(PersistenceError::Storage(format!(
                "cannot replace missing snapshot at sequence {sequence_nr} for {persistence_id}"
            )));
        };
        let actual_sequence = *actual_sequence;
        if actual_sequence != sequence_nr {
            return Err(PersistenceError::ConcurrencyViolation {
                expected: sequence_nr,
                actual: actual_sequence,
            });
        }
        if actual_snapshot.as_slice() != expected_snapshot {
            return Err(PersistenceError::Storage(format!(
                "snapshot changed while replacing sequence {sequence_nr} for {persistence_id}"
            )));
        }

        inner
            .snapshots
            .insert(persistence_id.to_string(), (sequence_nr, snapshot.to_vec()));
        inner
            .snapshot_history
            .entry(persistence_id.to_string())
            .or_default()
            .insert(sequence_nr, snapshot.to_vec());
        Ok(())
    }

    pub(super) async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let load_failure_prob = inner.faults.snapshot_load_failure_prob;
        if inner.rng.chance(load_failure_prob) {
            return Err(PersistenceError::Storage(
                "SimEventStore: injected snapshot load failure".into(),
            ));
        }
        Ok(inner.snapshots.get(persistence_id).cloned())
    }

    pub(super) async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let mut result = Vec::new();
        let mut seen = std::collections::BTreeSet::new();

        for (persistence_id, journal) in &inner.journals {
            if let Ok((t, entity_type, entity_id)) = parse_persistence_id_parts(persistence_id)
                && t == tenant
                && !journal.iter().any(is_entity_tombstone)
            {
                let key = (entity_type.to_string(), entity_id.to_string());
                if seen.insert(key.clone()) {
                    result.push(key);
                }
            }
        }

        Ok(result)
    }

    pub(super) async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let mut result = Vec::new();
        let mut seen = std::collections::BTreeSet::new();

        for (persistence_id, journal) in &inner.journals {
            if let Ok((t, found_type, entity_id)) = parse_persistence_id_parts(persistence_id)
                && t == tenant
                && found_type == entity_type
                && !journal.iter().any(is_entity_tombstone)
                && seen.insert(entity_id.to_string())
            {
                result.push(entity_id.to_string());
            }
        }

        Ok(result)
    }
}
