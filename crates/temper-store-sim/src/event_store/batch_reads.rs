//! Deterministic simulation event-store batch reads operations.

use super::*;

impl SimEventStore {
    pub(super) async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        if appends.is_empty() {
            return Ok(Vec::new());
        }

        let append_delay = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            appends.iter().find_map(|append| {
                let delay = inner
                    .pending_append_batch_delays
                    .get_mut(&append.persistence_id)
                    .and_then(VecDeque::pop_front);
                if inner
                    .pending_append_batch_delays
                    .get(&append.persistence_id)
                    .is_some_and(VecDeque::is_empty)
                {
                    inner
                        .pending_append_batch_delays
                        .remove(&append.persistence_id);
                }
                delay
            })
        };
        if let Some(delay) = append_delay
            && !delay.is_zero()
        {
            tokio::time::sleep(delay).await;
        }

        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        let mut seen = std::collections::BTreeSet::new();
        for append in appends {
            if !seen.insert(append.persistence_id.as_str()) {
                return Err(PersistenceError::Storage(format!(
                    "SimEventStore: duplicate persistence_id '{}' in append_batch",
                    append.persistence_id
                )));
            }
        }

        for append in appends {
            let pending_cv = inner
                .pending_concurrency_violations
                .get(&append.persistence_id)
                .copied()
                .unwrap_or(0);
            if pending_cv > 0 {
                if pending_cv == 1 {
                    inner
                        .pending_concurrency_violations
                        .remove(&append.persistence_id);
                } else {
                    inner
                        .pending_concurrency_violations
                        .insert(append.persistence_id.clone(), pending_cv - 1);
                }
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: append.expected_sequence,
                    actual: append.expected_sequence,
                });
            }
        }

        // Fault injection happens before mutation so a batch either writes
        // every stream or no stream.
        let cv_prob = inner.faults.concurrency_violation_prob;
        if inner.rng.chance(cv_prob) {
            let first = &appends[0];
            return Err(PersistenceError::ConcurrencyViolation {
                expected: first.expected_sequence,
                actual: first.expected_sequence.wrapping_add(1),
            });
        }
        let wf_prob = inner.faults.write_failure_prob;
        if inner.rng.chance(wf_prob) {
            return Err(PersistenceError::Storage(
                "SimEventStore: injected batch write failure".into(),
            ));
        }

        for append in appends {
            let current_seq = inner
                .journals
                .get(&append.persistence_id)
                .and_then(|journal| journal.last())
                .map(|event| event.sequence_nr)
                .unwrap_or(0);
            if current_seq != append.expected_sequence {
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: append.expected_sequence,
                    actual: current_seq,
                });
            }
        }

        let mut results = Vec::with_capacity(appends.len());
        for append in appends {
            let journal = inner
                .journals
                .entry(append.persistence_id.clone())
                .or_default();
            let mut new_seq = append.expected_sequence;
            for event in &append.events {
                new_seq += 1;
                let mut stored = event.clone();
                stored.sequence_nr = new_seq;
                journal.push(stored);
            }
            results.push(PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr: new_seq,
            });
        }

        Ok(results)
    }

    pub(super) async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.read_events_with_head(persistence_id, from_sequence)
            .await
            .map(|read| read.events)
    }

    pub(super) async fn read_events_with_head(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<JournalRead, PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        // Deterministic injected read failure (see `fail_next_reads`).
        if let Some(remaining) = inner.pending_read_failures.get_mut(persistence_id) {
            *remaining -= 1;
            let cleared = *remaining == 0;
            if cleared {
                inner.pending_read_failures.remove(persistence_id);
            }
            return Err(PersistenceError::Storage(format!(
                "injected read failure for {persistence_id}"
            )));
        }

        let journal = match inner.journals.get(persistence_id) {
            Some(journal) => journal,
            None => {
                return Ok(JournalRead {
                    events: Vec::new(),
                    journal_head_sequence_nr: 0,
                });
            }
        };
        let journal_head_sequence_nr = journal.last().map(|event| event.sequence_nr).unwrap_or(0);

        let mut events: Vec<PersistenceEnvelope> = journal
            .iter()
            .filter(|e| e.sequence_nr > from_sequence)
            .cloned()
            .collect();

        // Fault injection: truncate the returned events.
        let rt_prob = inner.faults.read_truncation_prob;
        if !events.is_empty() && inner.rng.chance(rt_prob) {
            let truncate_at = (inner.rng.next_u64() as usize) % events.len();
            events.truncate(truncate_at.max(1));
        }

        Ok(JournalRead {
            events,
            journal_head_sequence_nr,
        })
    }
}
