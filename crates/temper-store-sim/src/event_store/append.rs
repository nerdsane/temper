//! Deterministic simulation event-store append operations.

use super::*;

impl SimEventStore {
    pub(super) async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.append_with_index_rows(persistence_id, expected_sequence, events, &[], &[], false)
            .await
    }

    pub(super) async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
        vector_rows: &[EntityVectorRow],
        reconcile_vectors: bool,
    ) -> Result<u64, PersistenceError> {
        let append_delay = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let delay = inner
                .pending_append_delays
                .get_mut(persistence_id)
                .and_then(VecDeque::pop_front);
            if inner
                .pending_append_delays
                .get(persistence_id)
                .is_some_and(VecDeque::is_empty)
            {
                inner.pending_append_delays.remove(persistence_id);
            }
            delay
        };
        if let Some(delay) = append_delay
            && !delay.is_zero()
        {
            tokio::time::sleep(delay).await;
        }

        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        // Deterministic one-shot injection (see `inject_concurrency_violations`).
        // Consumes one counter per call; falls back to normal flow once drained.
        //
        // The reported `actual` equals `expected_sequence` — the journal has
        // not actually moved, so an authoritative replay will land back at
        // `expected_sequence`. Any code that asserts
        // `post_replay_sequence >= actual` still holds without this injection
        // lying about journal state.
        let pending_cv = inner
            .pending_concurrency_violations
            .get(persistence_id)
            .copied()
            .unwrap_or(0);
        if pending_cv > 0 {
            if pending_cv == 1 {
                inner.pending_concurrency_violations.remove(persistence_id);
            } else {
                inner
                    .pending_concurrency_violations
                    .insert(persistence_id.to_string(), pending_cv - 1);
            }
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: expected_sequence,
            });
        }

        // Fault injection: spurious concurrency violation (probabilistic).
        let cv_prob = inner.faults.concurrency_violation_prob;
        if inner.rng.chance(cv_prob) {
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: expected_sequence.wrapping_add(1),
            });
        }

        // Fault injection: write failure.
        let wf_prob = inner.faults.write_failure_prob;
        if inner.rng.chance(wf_prob) {
            return Err(PersistenceError::Storage(
                "SimEventStore: injected write failure".into(),
            ));
        }

        // Check optimistic concurrency.
        let current_seq = inner
            .journals
            .get(persistence_id)
            .and_then(|journal| journal.last().map(|e| e.sequence_nr))
            .unwrap_or(0);
        if current_seq != expected_sequence {
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: current_seq,
            });
        }

        // ADR-0153: validate declared-key uniqueness BEFORE writing the journal, so
        // a reject is atomic — the journal must not advance on a rejected co-commit.
        // A *different* entity already holding the key is the violation.
        if !key_rows.is_empty() {
            let mut parts = persistence_id.splitn(3, ':');
            let tenant = parts.next().unwrap_or("");
            let entity_type = parts.next().unwrap_or("");
            let entity_id = parts.next().unwrap_or("");
            for row in key_rows {
                if let Some(existing) = inner.key_index.get(&(
                    tenant.to_string(),
                    entity_type.to_string(),
                    row.key_name.clone(),
                    row.key_hash.clone(),
                )) && existing.as_str() != entity_id
                {
                    return Err(PersistenceError::Storage(format!(
                        "duplicate declared key '{}' for {entity_type}: held by {existing}",
                        row.key_name
                    )));
                }
            }
        }

        let mut new_seq = expected_sequence;
        let mut stored_events = Vec::with_capacity(events.len());
        for event in events {
            new_seq += 1;
            // Store with correct sequence number (ignore the one in the envelope,
            // use monotonic counter like the real stores do).
            let mut stored = event.clone();
            stored.sequence_nr = new_seq;
            stored_events.push(stored);
        }
        inner
            .journals
            .entry(persistence_id.to_string())
            .or_default()
            .extend(stored_events);

        let segments = inner
            .event_segments
            .entry(persistence_id.to_string())
            .or_insert_with(|| {
                vec![SimEventSegment {
                    segment_index: 0,
                    start_sequence_nr: (expected_sequence + 1).max(1),
                    end_sequence_nr: None,
                    snapshot_sequence: None,
                    event_count: 0,
                    sealed: false,
                }]
            });
        if segments.last().map(|s| s.sealed).unwrap_or(true) {
            let next_index = segments.last().map(|s| s.segment_index + 1).unwrap_or(0);
            segments.push(SimEventSegment {
                segment_index: next_index,
                start_sequence_nr: (expected_sequence + 1).max(1),
                end_sequence_nr: None,
                snapshot_sequence: None,
                event_count: 0,
                sealed: false,
            });
        }
        if new_seq > expected_sequence {
            let active_segment = segments
                .last_mut()
                .expect("segments must contain an active segment");
            active_segment.end_sequence_nr = Some(new_seq);
            active_segment.event_count = new_seq
                .saturating_sub(active_segment.start_sequence_nr)
                .saturating_add(1);
        }

        // ADR-0153: co-commit the declared key-index rows under the SAME lock as
        // the journal write above (uniqueness was validated before the journal, so
        // this only mutates — never fails). A keyed read is therefore consistent
        // with the journal: the negative-existence access path.
        if !key_rows.is_empty() {
            let mut parts = persistence_id.splitn(3, ':');
            let tenant = parts.next().unwrap_or("");
            let entity_type = parts.next().unwrap_or("");
            let entity_id = parts.next().unwrap_or("");
            for row in key_rows {
                // Drop the entity's prior row for this key_name (the value may have
                // changed), then claim the new (key_name, key_hash) -> entity_id.
                inner.key_index.retain(|(t, et, kn, _), eid| {
                    !(t.as_str() == tenant
                        && et.as_str() == entity_type
                        && kn.as_str() == row.key_name.as_str()
                        && eid.as_str() == entity_id)
                });
                inner.key_index.insert(
                    (
                        tenant.to_string(),
                        entity_type.to_string(),
                        row.key_name.clone(),
                        row.key_hash.clone(),
                    ),
                    entity_id.to_string(),
                );
            }
        }

        // ADR-0155: co-commit the derived vector-index rows under the SAME lock as
        // the journal write. When the entity's type declares vector paths
        // (`reconcile_vectors`), DELETE all of the entity's rows first, then insert
        // the current ones — so a delete transition or a cleared vector/model
        // property (empty `vector_rows`) purges the stale rows instead of leaving
        // them to rank forever. No uniqueness constraint — vectors are derived state.
        if reconcile_vectors {
            let mut parts = persistence_id.splitn(3, ':');
            let tenant = parts.next().unwrap_or("");
            let entity_type = parts.next().unwrap_or("");
            let entity_id = parts.next().unwrap_or("");
            inner.vector_index.retain(|(t, et, _, _, eid), _| {
                !(t.as_str() == tenant && et.as_str() == entity_type && eid.as_str() == entity_id)
            });
            for row in vector_rows {
                inner.vector_index.insert(
                    (
                        tenant.to_string(),
                        entity_type.to_string(),
                        row.decl_name.clone(),
                        row.model_tag.clone(),
                        entity_id.to_string(),
                    ),
                    row.vector.clone(),
                );
            }
        }

        Ok(new_seq)
    }
}
