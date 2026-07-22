//! Source-fenced deterministic single-stream appends.

use super::*;
use crate::source_fence::{
    append_retires_snapshot, snapshot_source_matches, update_segments_after_append_locked,
};

impl SimEventStore {
    pub(super) async fn append_with_index_rows_inner(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
        vector_rows: &[EntityVectorRow],
        reconciliation: IndexReconciliation,
    ) -> Result<u64, PersistenceError> {
        let (postcommit_pause, append_pause) = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let postcommit_pause = inner
                .pending_postcommit_append_pauses
                .get_mut(persistence_id)
                .and_then(VecDeque::pop_front);
            if inner
                .pending_postcommit_append_pauses
                .get(persistence_id)
                .is_some_and(VecDeque::is_empty)
            {
                inner
                    .pending_postcommit_append_pauses
                    .remove(persistence_id);
            }
            let pause = inner
                .pending_append_pauses
                .get_mut(persistence_id)
                .and_then(VecDeque::pop_front);
            if inner
                .pending_append_pauses
                .get(persistence_id)
                .is_some_and(VecDeque::is_empty)
            {
                inner.pending_append_pauses.remove(persistence_id);
            }
            (postcommit_pause, pause)
        };
        if let Some(pause) = append_pause {
            pause.reached.notify_one();
            pause.resume.notified().await;
        }
        let new_seq = {
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
            if !snapshot_source_matches(
                inner.snapshots.get(persistence_id),
                &reconciliation.snapshot_source,
            ) {
                return Err(PersistenceError::SnapshotGenerationChanged);
            }

            // Parse once before any journal mutation. This keeps key/vector ownership
            // aligned with the runtime's canonical persistence-id parser, including the
            // supported legacy two-segment form, and makes a malformed ID fail atomically.
            let index_identity =
                parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;

            // ADR-0153: validate declared-key uniqueness BEFORE writing the journal, so
            // a reject is atomic — the journal must not advance on a rejected co-commit.
            // A *different* entity already holding the key is the violation.
            if reconciliation.keys {
                let (tenant, entity_type, entity_id) = index_identity;
                for row in key_rows {
                    if let Some(existing) = inner.key_index.get(&(
                        tenant.to_string(),
                        entity_type.to_string(),
                        row.key_name.clone(),
                        row.key_hash.clone(),
                    )) && existing.0.as_str() != entity_id
                    {
                        return Err(PersistenceError::Storage(format!(
                            "duplicate declared key '{}' for {entity_type}: held by {}",
                            row.key_name, existing.0,
                        )));
                    }
                }
            }

            if reconciliation.keys {
                let (tenant, entity_type, _) = index_identity;
                reconcile_key_contract_locked(
                    &mut inner,
                    tenant,
                    entity_type,
                    reconciliation.key_set_signature.as_deref(),
                    KeyContractUse::LiveWrite,
                )?;
            } else if !events.is_empty() {
                let (tenant, entity_type, _) = index_identity;
                invalidate_coverage_for_unreconciled_append_locked(
                    &mut inner,
                    tenant,
                    entity_type,
                )?;
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

            update_segments_after_append_locked(
                &mut inner,
                persistence_id,
                expected_sequence,
                new_seq,
            );

            // ADR-0153/0171: co-commit the entity's EXACT declared key set under the
            // SAME lock as the journal write above (uniqueness was validated before the
            // journal, so this only mutates — never fails). Empty rows release every
            // prior claim, including rows for declarations that no longer exist.
            if reconciliation.keys {
                let (tenant, entity_type, entity_id) = index_identity;
                inner.key_index.retain(|(t, et, _, _), (eid, _)| {
                    !(t.as_str() == tenant
                        && et.as_str() == entity_type
                        && eid.as_str() == entity_id)
                });
                for row in key_rows {
                    inner.key_index.insert(
                        (
                            tenant.to_string(),
                            entity_type.to_string(),
                            row.key_name.clone(),
                            row.key_hash.clone(),
                        ),
                        (entity_id.to_string(), new_seq),
                    );
                }
            }

            // ADR-0155: co-commit the derived vector-index rows under the SAME lock as
            // the journal write. When the entity's type declares vector paths
            // (`reconciliation.vectors`), DELETE all of the entity's rows first, then insert
            // the current ones — so a delete transition or a cleared vector/model
            // property (empty `vector_rows`) purges the stale rows instead of leaving
            // them to rank forever. No uniqueness constraint — vectors are derived state.
            if reconciliation.vectors {
                let (tenant, entity_type, entity_id) = index_identity;
                inner.vector_index.retain(|(t, et, _, _, eid), _| {
                    !(t.as_str() == tenant
                        && et.as_str() == entity_type
                        && eid.as_str() == entity_id)
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

            if append_retires_snapshot(
                expected_sequence,
                events,
                &reconciliation.snapshot_source,
                index_identity.1,
                index_identity.2,
            ) {
                inner.snapshots.remove(persistence_id);
            }

            new_seq
        };

        // Model a lost/delayed acknowledgement after durability through an
        // explicit deterministic handshake. The store lock is already released,
        // so tests can prove retry behavior without ambient wall-clock races.
        if let Some(pause) = postcommit_pause {
            pause.reached.notify_one();
            pause.resume.notified().await;
        }

        Ok(new_seq)
    }
}
