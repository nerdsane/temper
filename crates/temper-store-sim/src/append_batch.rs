//! Atomic deterministic multi-stream appends.

use super::*;
use crate::source_fence::{
    append_retires_snapshot, snapshot_source_matches, update_segments_after_append_locked,
};

impl SimEventStore {
    pub(super) async fn append_batch_inner(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        if appends.is_empty() {
            return Ok(Vec::new());
        }

        let pause = self
            .inner
            .lock()
            .expect("SimEventStore lock poisoned")
            .pending_batch_pauses
            .pop_front();
        if let Some(pause) = pause {
            pause.reached.notify_one();
            pause.resume.notified().await;
        }

        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        let mut seen = std::collections::BTreeSet::new();
        let mut type_contracts = BTreeMap::new();
        let mut unreconciled_types = BTreeSet::new();
        let mut batch_claim = None;
        for append in appends {
            if !append.reconcile_keys && !append.key_rows.is_empty() {
                return Err(PersistenceError::Storage(format!(
                    "SimEventStore: append_batch stream '{}' supplied key rows without exact reconciliation",
                    append.persistence_id
                )));
            }
            if !seen.insert(append.persistence_id.as_str()) {
                return Err(PersistenceError::Storage(format!(
                    "SimEventStore: duplicate persistence_id '{}' in append_batch",
                    append.persistence_id
                )));
            }
            if let Some(claim) = &append.batch_idempotency
                && batch_claim.replace(claim).is_some()
            {
                return Err(PersistenceError::Storage(
                    "SimEventStore: append_batch supplied more than one idempotency claim"
                        .to_string(),
                ));
            }
            let (tenant, entity_type, _) = parse_persistence_id_parts(&append.persistence_id)
                .map_err(PersistenceError::Storage)?;
            if append.reconcile_keys {
                let type_key = (tenant.to_string(), entity_type.to_string());
                if let Some(existing) = type_contracts.get(&type_key) {
                    if existing != &append.key_set_signature {
                        return Err(PersistenceError::Storage(format!(
                            "SimEventStore: append_batch supplied inconsistent key contracts for {tenant}:{entity_type}"
                        )));
                    }
                } else {
                    type_contracts.insert(type_key, append.key_set_signature.clone());
                }
            } else if !append.events.is_empty() {
                unreconciled_types.insert((tenant.to_string(), entity_type.to_string()));
            }
        }

        if let Some(claim) = batch_claim {
            let claim_key = (claim.persistence_id.clone(), claim.idempotency_key.clone());
            if let Some(committed_hash) = inner.batch_idempotency.get(&claim_key) {
                if committed_hash != &claim.intent_hash {
                    return Err(PersistenceError::Storage(format!(
                        "atomic batch idempotency key '{}' was reused with a different intent",
                        claim.idempotency_key
                    )));
                }
                return Ok(appends
                    .iter()
                    .map(|append| PersistenceAppendResult {
                        persistence_id: append.persistence_id.clone(),
                        sequence_nr: inner
                            .journals
                            .get(&append.persistence_id)
                            .and_then(|journal| journal.last())
                            .map(|event| event.sequence_nr)
                            .unwrap_or(0),
                        batch_already_applied: true,
                    })
                    .collect());
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
            if !snapshot_source_matches(
                inner.snapshots.get(&append.persistence_id),
                &append.snapshot_source,
            ) {
                return Err(PersistenceError::SnapshotGenerationChanged);
            }
        }

        // Build the complete post-batch key map before mutating any journal. Removing
        // every participating entity first permits an atomic ownership transfer; a
        // conflicting final claim rejects the whole batch with both journals and the
        // live key map untouched.
        let mut next_key_index = inner.key_index.clone();
        for append in appends.iter().filter(|append| append.reconcile_keys) {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            next_key_index.retain(|(t, et, _, _), (owner, _)| {
                !(t.as_str() == tenant && et.as_str() == entity_type && owner.as_str() == entity_id)
            });
        }
        for append in appends.iter().filter(|append| append.reconcile_keys) {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            for row in &append.key_rows {
                let slot = (
                    tenant.to_string(),
                    entity_type.to_string(),
                    row.key_name.clone(),
                    row.key_hash.clone(),
                );
                if let Some(existing) = next_key_index.get(&slot)
                    && existing.0.as_str() != entity_id
                {
                    return Err(PersistenceError::Storage(format!(
                        "duplicate declared key '{}' for {entity_type}: held by {}",
                        row.key_name, existing.0
                    )));
                }
                let final_sequence = append
                    .expected_sequence
                    .checked_add(append.events.len() as u64)
                    .ok_or_else(|| {
                        PersistenceError::Storage(format!(
                            "append_batch sequence overflow for '{}'",
                            append.persistence_id
                        ))
                    })?;
                next_key_index.insert(slot, (entity_id.to_string(), final_sequence));
            }
        }

        let contract_before = inner.key_index_contract.clone();
        let watermark_before = inner.key_index_watermark.clone();
        for ((tenant, entity_type), signature) in &type_contracts {
            if let Err(error) = reconcile_key_contract_locked(
                &mut inner,
                tenant,
                entity_type,
                signature.as_deref(),
                KeyContractUse::LiveWrite,
            ) {
                inner.key_index_contract = contract_before;
                inner.key_index_watermark = watermark_before;
                return Err(error);
            }
        }
        for (tenant, entity_type) in &unreconciled_types {
            if let Err(error) =
                invalidate_coverage_for_unreconciled_append_locked(&mut inner, tenant, entity_type)
            {
                inner.key_index_contract = contract_before;
                inner.key_index_watermark = watermark_before;
                return Err(error);
            }
        }

        let mut results = Vec::with_capacity(appends.len());
        for append in appends {
            let mut new_seq = append.expected_sequence;
            {
                let journal = inner
                    .journals
                    .entry(append.persistence_id.clone())
                    .or_default();
                for event in &append.events {
                    new_seq += 1;
                    let mut stored = event.clone();
                    stored.sequence_nr = new_seq;
                    journal.push(stored);
                }
            }
            update_segments_after_append_locked(
                &mut inner,
                &append.persistence_id,
                append.expected_sequence,
                new_seq,
            );
            let (_, entity_type, entity_id) = parse_persistence_id_parts(&append.persistence_id)
                .map_err(PersistenceError::Storage)?;
            if append_retires_snapshot(
                append.expected_sequence,
                &append.events,
                &append.snapshot_source,
                entity_type,
                entity_id,
            ) {
                inner.snapshots.remove(&append.persistence_id);
            }
            results.push(PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr: new_seq,
                batch_already_applied: false,
            });
        }

        inner.key_index = next_key_index;
        if let Some(claim) = batch_claim {
            inner.batch_idempotency.insert(
                (claim.persistence_id.clone(), claim.idempotency_key.clone()),
                claim.intent_hash.clone(),
            );
        }

        Ok(results)
    }
}
