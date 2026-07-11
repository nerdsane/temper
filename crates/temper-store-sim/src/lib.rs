//! In-memory, deterministic event store for simulation testing.
//!
//! `SimEventStore` implements the [`EventStore`] trait using `BTreeMap` journals.
//! All operations resolve immediately and deterministically. Fault injection
//! is controlled by a seeded RNG for reproducible failures.
//!
//! This crate follows the FoundationDB pattern: swap the I/O, keep the code.
//! Server tests route this implementation through the `StorageStack`
//! event-journal capability so production actor code runs unchanged.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use temper_runtime::persistence::{
    EntityVectorCandidate, EntityVectorRow, EventStore, PersistenceAppend, PersistenceAppendResult,
    PersistenceEnvelope, PersistenceError, PersistenceSequenceGuard, contains_deletion_tombstone,
    ends_in_deletion_tombstone, validate_guarded_persistence_append_batch,
    validate_latest_event_batch,
};
use temper_runtime::tenant::parse_persistence_id_parts;

mod fault_control;
mod identity;
mod segments;
use identity::{canonical_persistence_id, canonical_test_persistence_id};
pub use segments::SimEventSegment;
use segments::{record_segment_append, rotate_for_snapshot};

/// Fault injection configuration for simulation.
///
/// Controls the probability of injected failures during event store operations.
/// All probabilities are in \[0.0, 1.0\].
#[derive(Debug, Clone)]
pub struct SimFaultConfig {
    /// Probability of a write failure on `append()`.
    pub write_failure_prob: f64,
    /// Probability of a spurious concurrency violation on `append()`.
    pub concurrency_violation_prob: f64,
    /// Probability of truncating journal on `read_events()`.
    pub read_truncation_prob: f64,
    /// Probability of snapshot save failure.
    pub snapshot_failure_prob: f64,
}

impl SimFaultConfig {
    /// No fault injection — all operations succeed.
    pub fn none() -> Self {
        Self {
            write_failure_prob: 0.0,
            concurrency_violation_prob: 0.0,
            read_truncation_prob: 0.0,
            snapshot_failure_prob: 0.0,
        }
    }

    /// Heavy fault injection for stress testing.
    pub fn heavy() -> Self {
        Self {
            write_failure_prob: 0.05,
            concurrency_violation_prob: 0.02,
            read_truncation_prob: 0.01,
            snapshot_failure_prob: 0.03,
        }
    }
}

impl Default for SimFaultConfig {
    fn default() -> Self {
        Self::none()
    }
}

/// Deterministic pseudo-random number generator for fault injection.
///
/// Simple xorshift64 — fast, deterministic, good enough for fault injection.
/// Uses `BTreeMap` internally (DST compliance: deterministic iteration order).
#[derive(Debug, Clone)]
pub struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    /// Create a new RNG with the given seed.
    pub fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 1 } else { seed },
        }
    }

    /// Generate next u64.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// Return true with the given probability \[0.0, 1.0\].
    pub fn chance(&mut self, prob: f64) -> bool {
        if prob <= 0.0 {
            return false;
        }
        if prob >= 1.0 {
            return true;
        }
        let threshold = (prob * u64::MAX as f64) as u64;
        self.next_u64() < threshold
    }
}

/// In-memory, deterministic event store for DST.
///
/// Implements `EventStore` trait. All operations resolve immediately.
/// Fault injection controlled by `DeterministicRng`.
///
/// Uses `BTreeMap` exclusively (no `HashMap`) for deterministic iteration order.
#[derive(Clone)]
pub struct SimEventStore {
    /// Event journals keyed by persistence_id.
    /// Each journal is an ordered list of envelopes.
    inner: Arc<Mutex<SimEventStoreInner>>,
}

#[derive(Debug)]
struct SimEventStoreInner {
    /// Event journals: persistence_id → Vec<PersistenceEnvelope>
    journals: BTreeMap<String, Vec<PersistenceEnvelope>>,
    /// Snapshots: persistence_id → (sequence_nr, snapshot_bytes)
    snapshots: BTreeMap<String, (u64, Vec<u8>)>,
    /// Immutable snapshot history: persistence_id → sequence_nr → snapshot bytes.
    snapshot_history: BTreeMap<String, BTreeMap<u64, Vec<u8>>>,
    /// Event segment metadata: persistence_id → Vec<SimEventSegment>.
    event_segments: BTreeMap<String, Vec<SimEventSegment>>,
    /// Fault injection RNG.
    rng: DeterministicRng,
    /// Fault injection configuration.
    faults: SimFaultConfig,
    /// One-shot concurrency-violation injection counters per `persistence_id`.
    ///
    /// Each entry tells `append` to return a `ConcurrencyViolation` on the next
    /// N calls for that id, then behave normally. Intended for deterministic
    /// retry-path tests where probabilistic injection would be flaky. See
    /// `inject_concurrency_violations`.
    pending_concurrency_violations: BTreeMap<String, u64>,
    /// Each entry makes `read_events` return a storage error on the next N calls for
    /// that id, then behave normally. Deterministic analogue to
    /// `pending_concurrency_violations`, for tests that need a journal-read failure
    /// (e.g. proving the key-index backfill treats an unreadable entity as
    /// `LoadFailed` and does not watermark its type). See `fail_next_reads`.
    pending_read_failures: BTreeMap<String, usize>,
    /// One-shot append delays per `persistence_id`.
    ///
    /// Used by dispatch retry tests to deterministically model "the actor
    /// persisted the transition, but the caller's ask timeout expired before
    /// the reply arrived".
    pending_append_delays: BTreeMap<String, VecDeque<Duration>>,
    /// ADR-0153: declared key-index, co-committed with the journal under the same
    /// lock. `(tenant, entity_type, key_name, key_hash) -> entity_id`. This is the
    /// deterministic reference for the negative-existence access path the real
    /// stores maintain in `entity_key_index`.
    key_index: BTreeMap<(String, String, String, String), String>,
    /// ADR-0153 backfill watermark: `(tenant, entity_type) -> key_set` — each completed
    /// type mapped to the sorted comma-joined declared key names the backfill covered.
    /// The deterministic reference for the real stores' `key_index_backfill_watermark`
    /// table — gates authoritative keyed absence, and detects a key-set change so a
    /// newly-declared key re-keys instead of being treated as already complete.
    key_index_watermark: BTreeMap<(String, String), String>,
    /// ADR-0155: derived vector index, co-committed with the journal under the same
    /// lock. `(tenant, entity_type, decl_name, model_tag, entity_id) -> vector`. The
    /// deterministic reference for the real stores' `entity_vector_index` — the
    /// exact-scan kNN access path. Unlike the key index this has no uniqueness
    /// constraint; it is derived, rebuildable ranking state.
    vector_index: BTreeMap<(String, String, String, String, String), Vec<f32>>,
    /// ADR-0155 backfill watermark: `(tenant, entity_type) -> vector_set` — each
    /// completed type mapped to the sorted comma-joined declared vector-path names the
    /// backfill covered. Mirrors `key_index_watermark`.
    vector_index_watermark: BTreeMap<(String, String), String>,
}

impl SimEventStore {
    /// Create a new SimEventStore with the given seed and fault config.
    pub fn new(seed: u64, faults: SimFaultConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SimEventStoreInner {
                journals: BTreeMap::new(),
                snapshots: BTreeMap::new(),
                snapshot_history: BTreeMap::new(),
                event_segments: BTreeMap::new(),
                rng: DeterministicRng::new(seed),
                faults,
                pending_concurrency_violations: BTreeMap::new(),
                pending_read_failures: BTreeMap::new(),
                pending_append_delays: BTreeMap::new(),
                key_index: BTreeMap::new(),
                key_index_watermark: BTreeMap::new(),
                vector_index: BTreeMap::new(),
                vector_index_watermark: BTreeMap::new(),
            })),
        }
    }

    /// Create a SimEventStore with no fault injection.
    pub fn no_faults(seed: u64) -> Self {
        Self::new(seed, SimFaultConfig::none())
    }

    /// Return the total number of events across all journals.
    pub fn total_events(&self) -> usize {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner.journals.values().map(|j| j.len()).sum()
    }

    /// Return the number of distinct persistence IDs with events.
    pub fn entity_count(&self) -> usize {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner.journals.len()
    }

    /// List all persistence IDs that have at least one event.
    ///
    /// Used by DST invariant checkers to iterate all entities in the store.
    pub fn list_all_persistence_ids(&self) -> Vec<String> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner.journals.keys().cloned().collect()
    }

    /// Temporarily disable all fault injection.
    ///
    /// Returns the previous config so it can be restored. Useful for
    /// restart phases where reads must succeed reliably.
    pub fn disable_faults(&self) -> SimFaultConfig {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let prev = inner.faults.clone();
        inner.faults = SimFaultConfig::none();
        prev
    }

    /// Restore a previously saved fault config.
    pub fn restore_faults(&self, faults: SimFaultConfig) {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner.faults = faults;
    }

    /// Dump all events for a persistence_id (for test assertions).
    pub fn dump_journal(&self, persistence_id: &str) -> Vec<PersistenceEnvelope> {
        let persistence_id = canonical_test_persistence_id(persistence_id);
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner
            .journals
            .get(&persistence_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn snapshot_history_len(&self, persistence_id: &str) -> usize {
        let persistence_id = canonical_test_persistence_id(persistence_id);
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner
            .snapshot_history
            .get(&persistence_id)
            .map(BTreeMap::len)
            .unwrap_or(0)
    }

    pub fn dump_segments(&self, persistence_id: &str) -> Vec<SimEventSegment> {
        let persistence_id = canonical_test_persistence_id(persistence_id);
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner
            .event_segments
            .get(&persistence_id)
            .cloned()
            .unwrap_or_default()
    }

    fn read_events_with_limit(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let persistence_id = canonical_persistence_id(persistence_id)?;
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        if let Some(remaining) = inner.pending_read_failures.get_mut(&persistence_id) {
            *remaining -= 1;
            let cleared = *remaining == 0;
            if cleared {
                inner.pending_read_failures.remove(&persistence_id);
            }
            return Err(PersistenceError::Storage(format!(
                "injected read failure for {persistence_id}"
            )));
        }

        let journal = match inner.journals.get(&persistence_id) {
            Some(journal) => journal,
            None => return Ok(Vec::new()),
        };
        let mut events = journal
            .iter()
            .filter(|event| event.sequence_nr > from_sequence)
            .take(limit)
            .cloned()
            .collect::<Vec<_>>();

        let read_truncation_probability = inner.faults.read_truncation_prob;
        if !events.is_empty() && inner.rng.chance(read_truncation_probability) {
            let truncate_at = (inner.rng.next_u64() as usize) % events.len();
            events.truncate(truncate_at.max(1));
        }

        Ok(events)
    }
}

impl std::fmt::Debug for SimEventStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        f.debug_struct("SimEventStore")
            .field("journals", &inner.journals.len())
            .field("snapshots", &inner.snapshots.len())
            .finish()
    }
}

impl EventStore for SimEventStore {
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.append_with_optional_keys(persistence_id, expected_sequence, events, None)
            .await
    }

    async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
        vector_rows: &[EntityVectorRow],
        reconcile_vectors: bool,
    ) -> Result<u64, PersistenceError> {
        let new_seq = self
            .append_with_optional_keys(persistence_id, expected_sequence, events, Some(key_rows))
            .await?;
        if reconcile_vectors {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
            self.backfill_entity_vectors(tenant, entity_type, entity_id, vector_rows)
                .await?;
        }
        Ok(new_seq)
    }

    async fn append_with_optional_keys(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: Option<&[temper_runtime::persistence::EntityKeyRow]>,
    ) -> Result<u64, PersistenceError> {
        if events.is_empty() {
            return Ok(expected_sequence);
        }

        let (tenant, entity_type, entity_id) =
            parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
        let canonical_id = format!("{tenant}:{entity_type}:{entity_id}");
        let contains_deletion = contains_deletion_tombstone(events);
        let ends_in_deletion = ends_in_deletion_tombstone(events);
        let replaces_keys = key_rows.is_some();
        let key_rows = key_rows.unwrap_or_default();
        let reconcile_vectors = false;
        let vector_rows: &[EntityVectorRow] = &[];

        let append_delay = {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let delay = inner
                .pending_append_delays
                .get_mut(&canonical_id)
                .and_then(VecDeque::pop_front);
            if inner
                .pending_append_delays
                .get(&canonical_id)
                .is_some_and(VecDeque::is_empty)
            {
                inner.pending_append_delays.remove(&canonical_id);
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
            .get(&canonical_id)
            .copied()
            .unwrap_or(0);
        if pending_cv > 0 {
            if pending_cv == 1 {
                inner.pending_concurrency_violations.remove(&canonical_id);
            } else {
                inner
                    .pending_concurrency_violations
                    .insert(canonical_id.clone(), pending_cv - 1);
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
            .get(&canonical_id)
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
        if !ends_in_deletion && !key_rows.is_empty() {
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
            new_seq = new_seq.checked_add(1).ok_or_else(|| {
                PersistenceError::Storage(format!(
                    "event sequence exhausted while appending {persistence_id}"
                ))
            })?;
            // Store with correct sequence number (ignore the one in the envelope,
            // use monotonic counter like the real stores do).
            let mut stored = event.clone();
            stored.sequence_nr = new_seq;
            stored_events.push(stored);
        }
        record_segment_append(
            &mut inner.event_segments,
            &canonical_id,
            expected_sequence,
            new_seq,
        )?;
        inner
            .journals
            .entry(canonical_id)
            .or_default()
            .extend(stored_events);

        if contains_deletion || replaces_keys {
            // Raw appends preserve existing claims; only an explicit complete
            // replacement or a tombstone may retire them.
            inner.key_index.retain(|(t, et, _, _), eid| {
                !(t.as_str() == tenant && et.as_str() == entity_type && eid.as_str() == entity_id)
            });
            if !ends_in_deletion {
                for row in key_rows {
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

    async fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
    ) -> Result<(), PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        for row in key_rows {
            let slot = (
                tenant.to_string(),
                entity_type.to_string(),
                row.key_name.clone(),
                row.key_hash.clone(),
            );
            if let Some(existing) = inner.key_index.get(&slot)
                && existing.as_str() != entity_id
            {
                return Err(PersistenceError::Storage(format!(
                    "declared-key conflict for '{}': {entity_type}('{entity_id}') conflicts with '{existing}'",
                    row.key_name
                )));
            }
        }
        inner.key_index.retain(|(t, et, _, _), existing_id| {
            !(t.as_str() == tenant
                && et.as_str() == entity_type
                && existing_id.as_str() == entity_id)
        });
        for row in key_rows {
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
        Ok(())
    }

    async fn lookup_by_key(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let slot = (
            tenant.to_string(),
            entity_type.to_string(),
            key_name.to_string(),
            key_hash.to_string(),
        );
        Ok(inner.key_index.get(&slot).cloned())
    }

    async fn mark_key_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> Result<(), PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        // Overwrite the covered key-set (a re-key after a key-set change replaces the
        // stale set), mirroring the Postgres upsert.
        inner.key_index_watermark.insert(
            (tenant.to_string(), entity_type.to_string()),
            key_set.to_string(),
        );
        Ok(())
    }

    async fn key_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(inner
            .key_index_watermark
            .iter()
            .filter(|((t, _), _)| t.as_str() == tenant)
            .map(|((_, et), key_set)| (et.clone(), key_set.clone()))
            .collect())
    }

    async fn keyed_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let mut ids: BTreeSet<String> = BTreeSet::new();
        for ((t, et, _, _), entity_id) in inner.key_index.iter() {
            if t.as_str() == tenant && et.as_str() == entity_type {
                ids.insert(entity_id.clone());
            }
        }
        Ok(ids.into_iter().collect())
    }

    async fn backfill_entity_vectors(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        vector_rows: &[EntityVectorRow],
    ) -> Result<(), PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        // Reconcile: drop ALL of the entity's rows, then insert the current ones.
        // Empty `vector_rows` purges the entity (deleted / un-embedded). Idempotent.
        inner.vector_index.retain(|(t, et, _, _, eid), _| {
            !(t.as_str() == tenant && et.as_str() == entity_type && eid == entity_id)
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
        Ok(())
    }

    async fn vector_candidates(
        &self,
        tenant: &str,
        entity_type: &str,
        decl_name: &str,
        model_tag: &str,
        limit: usize,
    ) -> Result<Vec<EntityVectorCandidate>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        // BTreeMap iteration is ordered by key, so `entity_id` (the last key
        // component within a fixed partition) yields deterministic candidate order.
        // Cap at `limit` so an over-budget partition is detected without copying it all.
        let mut out = Vec::new();
        for ((t, et, decl, tag, entity_id), vector) in inner.vector_index.iter() {
            if t.as_str() == tenant
                && et.as_str() == entity_type
                && decl.as_str() == decl_name
                && tag.as_str() == model_tag
            {
                if out.len() >= limit {
                    break;
                }
                out.push(EntityVectorCandidate {
                    entity_id: entity_id.clone(),
                    vector: vector.clone(),
                });
            }
        }
        Ok(out)
    }

    async fn mark_vector_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        vector_set: &str,
    ) -> Result<(), PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner.vector_index_watermark.insert(
            (tenant.to_string(), entity_type.to_string()),
            vector_set.to_string(),
        );
        Ok(())
    }

    async fn vector_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(inner
            .vector_index_watermark
            .iter()
            .filter(|((t, _), _)| t.as_str() == tenant)
            .map(|((_, et), vector_set)| (et.clone(), vector_set.clone()))
            .collect())
    }

    async fn vectored_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let mut ids: BTreeSet<String> = BTreeSet::new();
        for ((t, et, _, _, entity_id), _) in inner.vector_index.iter() {
            if t.as_str() == tenant && et.as_str() == entity_type {
                ids.insert(entity_id.clone());
            }
        }
        Ok(ids.into_iter().collect())
    }

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        EventStore::append_batch_guarded(self, appends, &[]).await
    }

    async fn append_batch_guarded(
        &self,
        appends: &[PersistenceAppend],
        guards: &[PersistenceSequenceGuard],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        validate_guarded_persistence_append_batch(appends, guards)?;
        if appends.is_empty() {
            return Ok(Vec::new());
        }

        let canonical_ids = appends
            .iter()
            .map(|append| canonical_persistence_id(&append.persistence_id))
            .collect::<Result<Vec<_>, _>>()?;
        let canonical_guards = guards
            .iter()
            .map(|guard| canonical_persistence_id(&guard.persistence_id))
            .collect::<Result<Vec<_>, _>>()?;
        if appends.iter().all(|append| append.events.is_empty()) {
            return Ok(appends
                .iter()
                .map(|append| PersistenceAppendResult {
                    persistence_id: append.persistence_id.clone(),
                    sequence_nr: append.expected_sequence,
                })
                .collect());
        }

        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        for (guard, canonical_id) in guards.iter().zip(&canonical_guards) {
            let actual = inner
                .journals
                .get(canonical_id)
                .and_then(|journal| journal.last())
                .map(|event| event.sequence_nr)
                .unwrap_or(0);
            if actual != guard.expected_sequence {
                return Err(PersistenceError::PreconditionFailed {
                    persistence_id: guard.persistence_id.clone(),
                    expected: guard.expected_sequence,
                    actual,
                });
            }
        }

        for (append, canonical_id) in appends.iter().zip(&canonical_ids) {
            if append.events.is_empty() {
                continue;
            }
            let pending_cv = inner
                .pending_concurrency_violations
                .get(canonical_id)
                .copied()
                .unwrap_or(0);
            if pending_cv > 0 {
                if pending_cv == 1 {
                    inner.pending_concurrency_violations.remove(canonical_id);
                } else {
                    inner
                        .pending_concurrency_violations
                        .insert(canonical_id.clone(), pending_cv - 1);
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

        for (append, canonical_id) in appends.iter().zip(&canonical_ids) {
            if append.events.is_empty() {
                continue;
            }
            let current_seq = inner
                .journals
                .get(canonical_id)
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

        // Build the complete post-batch key map before mutating journals. This
        // validates uniqueness across both existing holders and other streams
        // in the same atomic batch. A raw append (`key_rows == None`) preserves
        // claims it cannot recompute; an explicit replacement or tombstone
        // retires the prior set.
        let mut next_key_index = inner.key_index.clone();
        for append in appends.iter().filter(|append| {
            !append.events.is_empty()
                && (append.key_rows.is_some() || contains_deletion_tombstone(&append.events))
        }) {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            next_key_index.retain(|(t, et, _, _), existing_id| {
                !(t.as_str() == tenant
                    && et.as_str() == entity_type
                    && existing_id.as_str() == entity_id)
            });
        }
        for append in appends.iter().filter(|append| {
            !append.events.is_empty() && !ends_in_deletion_tombstone(&append.events)
        }) {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            for row in append.key_rows.as_deref().unwrap_or_default() {
                let key = (
                    tenant.to_string(),
                    entity_type.to_string(),
                    row.key_name.clone(),
                    row.key_hash.clone(),
                );
                if let Some(existing) = next_key_index.get(&key)
                    && existing.as_str() != entity_id
                {
                    return Err(PersistenceError::Storage(format!(
                        "duplicate declared key '{}' for {entity_type}: held by {existing}",
                        row.key_name
                    )));
                }
                next_key_index.insert(key, entity_id.to_string());
            }
        }

        let mut next_segments = inner.event_segments.clone();
        let mut prepared_events = BTreeMap::new();
        let mut planned_sequences = BTreeMap::new();
        for (append, canonical_id) in appends.iter().zip(&canonical_ids) {
            if append.events.is_empty() {
                planned_sequences.insert(canonical_id.clone(), append.expected_sequence);
                continue;
            }
            let mut new_seq = append.expected_sequence;
            let mut stored_events = Vec::with_capacity(append.events.len());
            for event in &append.events {
                new_seq = new_seq.checked_add(1).ok_or_else(|| {
                    PersistenceError::Storage(format!(
                        "event sequence exhausted while appending {}",
                        append.persistence_id
                    ))
                })?;
                let mut stored = event.clone();
                stored.sequence_nr = new_seq;
                stored_events.push(stored);
            }
            record_segment_append(
                &mut next_segments,
                canonical_id,
                append.expected_sequence,
                new_seq,
            )?;
            prepared_events.insert(canonical_id.clone(), stored_events);
            planned_sequences.insert(canonical_id.clone(), new_seq);
        }

        for (persistence_id, events) in prepared_events {
            inner
                .journals
                .entry(persistence_id)
                .or_default()
                .extend(events);
        }

        inner.event_segments = next_segments;
        inner.key_index = next_key_index;

        Ok(appends
            .iter()
            .zip(canonical_ids)
            .map(|(append, canonical_id)| PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr: planned_sequences[&canonical_id],
            })
            .collect())
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.read_events_with_limit(persistence_id, from_sequence, usize::MAX)
    }

    async fn read_events_bounded(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.read_events_with_limit(persistence_id, from_sequence, limit)
    }

    async fn read_latest_events(
        &self,
        persistence_ids: &[String],
    ) -> Result<Vec<Option<PersistenceEnvelope>>, PersistenceError> {
        validate_latest_event_batch(persistence_ids)?;
        let canonical_ids = persistence_ids
            .iter()
            .map(|persistence_id| canonical_persistence_id(persistence_id))
            .collect::<Result<Vec<_>, _>>()?;
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        for (persistence_id, canonical_id) in persistence_ids.iter().zip(&canonical_ids) {
            if let Some(remaining) = inner.pending_read_failures.get_mut(canonical_id) {
                *remaining -= 1;
                let cleared = *remaining == 0;
                if cleared {
                    inner.pending_read_failures.remove(canonical_id);
                }
                return Err(PersistenceError::Storage(format!(
                    "injected latest-event read failure for {persistence_id}"
                )));
            }
        }

        Ok(canonical_ids
            .iter()
            .map(|canonical_id| {
                inner
                    .journals
                    .get(canonical_id)
                    .and_then(|journal| journal.last())
                    .cloned()
            })
            .collect())
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        let persistence_id = canonical_persistence_id(persistence_id)?;
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        // Fault injection: snapshot save failure.
        let sf_prob = inner.faults.snapshot_failure_prob;
        if inner.rng.chance(sf_prob) {
            return Err(PersistenceError::Storage(
                "SimEventStore: injected snapshot failure".into(),
            ));
        }

        let journal_tail = inner
            .journals
            .get(&persistence_id)
            .and_then(|journal| journal.last())
            .map_or(0, |event| event.sequence_nr);
        let next_segments = rotate_for_snapshot(
            &inner.event_segments,
            &persistence_id,
            journal_tail,
            sequence_nr,
        )?;

        if inner
            .snapshots
            .get(&persistence_id)
            .is_none_or(|(current_sequence, _)| sequence_nr >= *current_sequence)
        {
            inner
                .snapshots
                .insert(persistence_id.clone(), (sequence_nr, snapshot.to_vec()));
        }
        inner
            .snapshot_history
            .entry(persistence_id.clone())
            .or_default()
            .insert(sequence_nr, snapshot.to_vec());
        if let Some(segments) = next_segments {
            inner.event_segments.insert(persistence_id, segments);
        }
        Ok(())
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let persistence_id = canonical_persistence_id(persistence_id)?;
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(inner.snapshots.get(&persistence_id).cloned())
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let mut result = Vec::new();
        let mut seen = std::collections::BTreeSet::new();

        for persistence_id in inner.journals.keys() {
            if let Ok((t, entity_type, entity_id)) = parse_persistence_id_parts(persistence_id)
                && t == tenant
            {
                let key = (entity_type.to_string(), entity_id.to_string());
                if seen.insert(key.clone()) {
                    result.push(key);
                }
            }
        }

        Ok(result)
    }

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let mut result = Vec::new();
        let mut seen = std::collections::BTreeSet::new();

        for persistence_id in inner.journals.keys() {
            if let Ok((t, found_type, entity_id)) = parse_persistence_id_parts(persistence_id)
                && t == tenant
                && found_type == entity_type
                && seen.insert(entity_id.to_string())
            {
                result.push(entity_id.to_string());
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod key_test;
#[cfg(test)]
mod snapshot_test;
#[cfg(test)]
mod tests;
