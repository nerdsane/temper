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

use temper_runtime::persistence::schema_deployment::{
    SchemaExecutionPin, SchemaScope, scoped_journal_pin_suffix, split_scoped_journal_entity_id,
};
use temper_runtime::persistence::{
    EntityVectorCandidate, EntityVectorRow, EventStore, PersistenceAppend, PersistenceAppendResult,
    PersistenceEnvelope, PersistenceError,
};
use temper_runtime::tenant::parse_persistence_id_parts;

mod schema_deployment;
pub use schema_deployment::SimSchemaFaultPoint;

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
    /// Deterministic schema-deployment authority state.
    schema_deployments: schema_deployment::SimSchemaDeploymentState,
    /// Durable tenant-global stream publication fences by entity type.
    unscoped_stream_fences: BTreeMap<(String, String), (String, String, String, String)>,
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
    /// One-shot deterministic failures for schema lifecycle transactions.
    pending_schema_failures: BTreeMap<SimSchemaFaultPoint, u64>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimEventSegment {
    pub segment_index: u64,
    pub start_sequence_nr: u64,
    pub end_sequence_nr: Option<u64>,
    pub snapshot_sequence: Option<u64>,
    pub event_count: u64,
    pub sealed: bool,
}

impl SimEventStore {
    /// Create a new SimEventStore with the given seed and fault config.
    pub fn new(seed: u64, faults: SimFaultConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SimEventStoreInner {
                schema_deployments: schema_deployment::SimSchemaDeploymentState::default(),
                unscoped_stream_fences: BTreeMap::new(),
                journals: BTreeMap::new(),
                snapshots: BTreeMap::new(),
                snapshot_history: BTreeMap::new(),
                event_segments: BTreeMap::new(),
                rng: DeterministicRng::new(seed),
                faults,
                pending_schema_failures: BTreeMap::new(),
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

    /// Inject exactly `count` deterministic `ConcurrencyViolation` errors on
    /// the next `count` `append` calls for `persistence_id`, then behave
    /// normally.
    ///
    /// Use this for retry-path tests where the probabilistic fault injection
    /// in `SimFaultConfig` would be flaky. Each injected violation reports
    /// `actual = expected_sequence` (the journal has not actually moved), so
    /// any callers with post-replay sequence assertions still hold after the
    /// retry replays back to the same spot.
    pub fn inject_concurrency_violations(&self, persistence_id: &str, count: u64) {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        if count == 0 {
            inner.pending_concurrency_violations.remove(persistence_id);
        } else {
            inner
                .pending_concurrency_violations
                .insert(persistence_id.to_string(), count);
        }
    }

    /// Make the next `count` `read_events` calls for `persistence_id` fail with a
    /// storage error, then behave normally. Deterministic (unlike
    /// `read_truncation_prob`) so tests can prove read-failure handling — e.g. that
    /// the key-index backfill classifies an unreadable entity as `LoadFailed` and
    /// therefore does not watermark its type. `count == 0` clears the injection.
    pub fn fail_next_reads(&self, persistence_id: &str, count: usize) {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        if count == 0 {
            inner.pending_read_failures.remove(persistence_id);
        } else {
            inner
                .pending_read_failures
                .insert(persistence_id.to_string(), count);
        }
    }

    /// Return the current count of pending injected concurrency violations for
    /// `persistence_id`. Zero if none are queued.
    pub fn pending_concurrency_violations(&self, persistence_id: &str) -> u64 {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner
            .pending_concurrency_violations
            .get(persistence_id)
            .copied()
            .unwrap_or(0)
    }

    /// Delay the next append for `persistence_id` by `delay`.
    ///
    /// The delay is consumed once. Multiple calls queue multiple delays in
    /// FIFO order.
    pub fn inject_append_delay(&self, persistence_id: &str, delay: Duration) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .pending_append_delays
            .entry(persistence_id.to_string())
            .or_default()
            .push_back(delay);
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
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner
            .journals
            .get(persistence_id)
            .cloned()
            .unwrap_or_default()
    }

    pub fn snapshot_history_len(&self, persistence_id: &str) -> usize {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner
            .snapshot_history
            .get(persistence_id)
            .map(BTreeMap::len)
            .unwrap_or(0)
    }

    pub fn dump_segments(&self, persistence_id: &str) -> Vec<SimEventSegment> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner
            .event_segments
            .get(persistence_id)
            .cloned()
            .unwrap_or_default()
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
        self.append_with_index_rows(persistence_id, expected_sequence, events, &[], &[], false)
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

        if let Ok((tenant, entity_type, entity_id)) = parse_persistence_id_parts(persistence_id)
            && let Some((_, pin)) = split_scoped_journal_entity_id(entity_id)
        {
            if inner.schema_deployments.migrated_source_is_fenced(
                tenant,
                &pin.scope,
                &pin.bundle_digest,
            ) {
                return Err(PersistenceError::Storage(
                    "migrated scoped schema write fence".into(),
                ));
            }
            if let Some(publication_action) =
                inner.schema_deployments.scoped_stream_publication_action(
                    tenant,
                    &pin.scope,
                    &pin.bundle_digest,
                    entity_type,
                )
                && events.iter().any(|event| {
                    event.event_type == publication_action && event.metadata.kernel.is_none()
                })
            {
                return Err(PersistenceError::Storage(
                    "stream descriptor source publication fence".into(),
                ));
            }
            if !inner.journals.contains_key(persistence_id)
                && !inner.schema_deployments.permits_scoped_journal_write(
                    tenant,
                    &pin.scope,
                    &pin.bundle_digest,
                )
            {
                return Err(PersistenceError::Storage(
                    "stale scoped schema write fence".into(),
                ));
            }
        }
        if let Ok((tenant, entity_type, entity_id)) = parse_persistence_id_parts(persistence_id)
            && split_scoped_journal_entity_id(entity_id).is_none()
            && let Some((_, _, publication_action, _)) = inner
                .unscoped_stream_fences
                .get(&(tenant.to_string(), entity_type.to_string()))
            && events.iter().any(|event| {
                event.event_type == *publication_action && event.metadata.kernel.is_none()
            })
        {
            return Err(PersistenceError::Storage(
                "stream descriptor publication fence".into(),
            ));
        }

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
            match inner.key_index.get(&slot) {
                // A different entity holds it — pre-existing conflict; skip (don't
                // clobber, don't fail the backfill).
                Some(existing) if existing.as_str() != entity_id => continue,
                _ => {
                    inner.key_index.retain(|(t, et, kn, _), eid| {
                        !(t.as_str() == tenant
                            && et.as_str() == entity_type
                            && kn.as_str() == row.key_name.as_str()
                            && eid.as_str() == entity_id)
                    });
                    inner.key_index.insert(slot, entity_id.to_string());
                }
            }
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
        if appends.is_empty() {
            return Ok(Vec::new());
        }

        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        let mut seen = std::collections::BTreeSet::new();
        let mut batch_key_claims = std::collections::BTreeMap::new();
        for append in appends {
            if !seen.insert(append.persistence_id.as_str()) {
                return Err(PersistenceError::Storage(format!(
                    "SimEventStore: duplicate persistence_id '{}' in append_batch",
                    append.persistence_id
                )));
            }
        }

        for append in appends {
            if let Ok((tenant, entity_type, entity_id)) =
                parse_persistence_id_parts(&append.persistence_id)
                && let Some((_, pin)) = split_scoped_journal_entity_id(entity_id)
            {
                if inner.schema_deployments.migrated_source_is_fenced(
                    tenant,
                    &pin.scope,
                    &pin.bundle_digest,
                ) {
                    return Err(PersistenceError::Storage(
                        "migrated scoped schema write fence".into(),
                    ));
                }
                if let Some(publication_action) =
                    inner.schema_deployments.scoped_stream_publication_action(
                        tenant,
                        &pin.scope,
                        &pin.bundle_digest,
                        entity_type,
                    )
                    && append.events.iter().any(|event| {
                        event.event_type == publication_action && event.metadata.kernel.is_none()
                    })
                {
                    return Err(PersistenceError::Storage(
                        "stream descriptor source publication fence".into(),
                    ));
                }
                if !inner.journals.contains_key(&append.persistence_id)
                    && !inner.schema_deployments.permits_scoped_journal_write(
                        tenant,
                        &pin.scope,
                        &pin.bundle_digest,
                    )
                {
                    return Err(PersistenceError::Storage(
                        "stale scoped schema write fence".into(),
                    ));
                }
            }
            if let Ok((tenant, entity_type, entity_id)) =
                parse_persistence_id_parts(&append.persistence_id)
                && split_scoped_journal_entity_id(entity_id).is_none()
                && let Some((_, _, publication_action, _)) = inner
                    .unscoped_stream_fences
                    .get(&(tenant.to_string(), entity_type.to_string()))
                && append.events.iter().any(|event| {
                    event.event_type == *publication_action && event.metadata.kernel.is_none()
                })
            {
                return Err(PersistenceError::Storage(
                    "stream descriptor publication fence".into(),
                ));
            }
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
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            for row in &append.key_rows {
                let key = (
                    tenant.to_string(),
                    entity_type.to_string(),
                    row.key_name.clone(),
                    row.key_hash.clone(),
                );
                let existing = batch_key_claims
                    .get(&key)
                    .or_else(|| inner.key_index.get(&key));
                if existing.is_some_and(|existing| existing != entity_id) {
                    return Err(PersistenceError::Storage(format!(
                        "duplicate declared key '{}' for {entity_type}: held by {existing}",
                        row.key_name,
                        existing = existing.expect("checked as present")
                    )));
                }
                batch_key_claims.insert(key, entity_id.to_string());
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
        for append in appends {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            for row in &append.key_rows {
                inner.key_index.retain(|(t, et, name, _), holder| {
                    !(t.as_str() == tenant
                        && et == entity_type
                        && name == &row.key_name
                        && holder == entity_id)
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
            if append.reconcile_vectors {
                inner.vector_index.retain(|(t, et, _, _, id), _| {
                    !(t.as_str() == tenant && et == entity_type && id == entity_id)
                });
                for row in &append.vector_rows {
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
        }

        Ok(results)
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
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
            Some(j) => j,
            None => return Ok(Vec::new()),
        };

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

        Ok(events)
    }

    async fn read_events_limited(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
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
        let Some(journal) = inner.journals.get(persistence_id) else {
            return Ok(Vec::new());
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

    async fn read_latest_events(
        &self,
        persistence_id: &str,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
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
        let Some(journal) = inner.journals.get(persistence_id) else {
            return Ok(Vec::new());
        };
        let start = journal.len().saturating_sub(limit);
        let mut events = journal[start..].to_vec();
        let read_truncation_probability = inner.faults.read_truncation_prob;
        if !events.is_empty() && inner.rng.chance(read_truncation_probability) {
            let truncate_at = (inner.rng.next_u64() as usize) % events.len();
            events.truncate(truncate_at.max(1));
        }
        Ok(events)
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
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

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(inner.snapshots.get(persistence_id).cloned())
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

    async fn list_journal_ids_page(
        &self,
        tenant: &str,
        entity_type: Option<&str>,
        after: Option<(&str, &str)>,
        limit: usize,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let result = inner
            .journals
            .keys()
            .filter_map(|persistence_id| {
                parse_persistence_id_parts(persistence_id)
                    .ok()
                    .filter(|(found_tenant, _, _)| *found_tenant == tenant)
                    .map(|(_, entity_type, entity_id)| {
                        (entity_type.to_string(), entity_id.to_string())
                    })
            })
            .filter(|(found_type, _)| entity_type.is_none_or(|wanted| found_type == wanted))
            .filter(|(entity_type, entity_id)| {
                after.is_none_or(|cursor| (entity_type.as_str(), entity_id.as_str()) > cursor)
            })
            .take(limit)
            .collect::<Vec<_>>();
        Ok(result)
    }

    async fn list_unscoped_entity_ids_page(
        &self,
        tenant: &str,
        entity_type: &str,
        after_entity_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(inner
            .journals
            .keys()
            .filter_map(|persistence_id| {
                parse_persistence_id_parts(persistence_id)
                    .ok()
                    .filter(|(found_tenant, found_type, entity_id)| {
                        *found_tenant == tenant
                            && *found_type == entity_type
                            && split_scoped_journal_entity_id(entity_id).is_none()
                    })
                    .map(|(_, _, entity_id)| entity_id.to_string())
            })
            .filter(|entity_id| after_entity_id.is_none_or(|after| entity_id.as_str() > after))
            .take(limit)
            .collect())
    }

    async fn unscoped_entity_type_write_version(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<u64, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner
            .journals
            .iter()
            .filter_map(|(persistence_id, events)| {
                parse_persistence_id_parts(persistence_id)
                    .ok()
                    .filter(|(found_tenant, found_type, entity_id)| {
                        *found_tenant == tenant
                            && *found_type == entity_type
                            && split_scoped_journal_entity_id(entity_id).is_none()
                    })
                    .map(|_| events.len())
            })
            .try_fold(0_u64, |total, count| {
                u64::try_from(count)
                    .ok()
                    .and_then(|count| total.checked_add(count))
            })
            .ok_or_else(|| PersistenceError::Storage("global write version exhausted".into()))
    }

    async fn activate_unscoped_stream_publication_fence(
        &self,
        tenant: &str,
        fence: &temper_runtime::persistence::schema_deployment::StreamPublicationFence,
    ) -> Result<(), PersistenceError> {
        let temper_runtime::persistence::schema_deployment::StreamPublicationFence::InstalledApplication {
            application_id,
            semantic_digest,
            bindings,
        } = fence
        else {
            return Err(PersistenceError::Storage(
                "task fence cannot activate global stream publications".into(),
            ));
        };
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        for (entity_type, binding) in bindings {
            let version = inner
                .journals
                .iter()
                .filter_map(|(persistence_id, events)| {
                    parse_persistence_id_parts(persistence_id)
                        .ok()
                        .filter(|(found_tenant, found_type, entity_id)| {
                            *found_tenant == tenant
                                && *found_type == entity_type
                                && split_scoped_journal_entity_id(entity_id).is_none()
                        })
                        .map(|_| events.len())
                })
                .try_fold(0_u64, |total, count| {
                    u64::try_from(count)
                        .ok()
                        .and_then(|count| total.checked_add(count))
                })
                .ok_or_else(|| {
                    PersistenceError::Storage("global write version exhausted".into())
                })?;
            if version != binding.expected_write_version {
                return Err(PersistenceError::ConcurrencyViolation {
                    expected: binding.expected_write_version,
                    actual: version,
                });
            }
        }
        inner
            .unscoped_stream_fences
            .retain(|(found_tenant, _), (found_application, _, _, _)| {
                found_tenant != tenant || found_application != application_id
            });
        for (entity_type, binding) in bindings {
            inner.unscoped_stream_fences.insert(
                (tenant.to_string(), entity_type.clone()),
                (
                    application_id.clone(),
                    semantic_digest.clone(),
                    binding.publication_action.clone(),
                    binding.capability_digest.clone(),
                ),
            );
        }
        Ok(())
    }

    async fn deactivate_unscoped_stream_publication_fence(
        &self,
        tenant: &str,
        application_id: &str,
        semantic_digest: Option<&str>,
    ) -> Result<(), PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner.unscoped_stream_fences.retain(
            |(found_tenant, _), (found_application, found_digest, _, _)| {
                found_tenant != tenant
                    || found_application != application_id
                    || semantic_digest.is_some_and(|expected| found_digest != expected)
            },
        );
        Ok(())
    }

    async fn get_unscoped_stream_publication_fence(
        &self,
        tenant: &str,
        application_id: &str,
    ) -> Result<
        Option<temper_runtime::persistence::schema_deployment::StreamPublicationFence>,
        PersistenceError,
    > {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let mut semantic_digest = None;
        let mut bindings = BTreeMap::new();
        for (
            (found_tenant, entity_type),
            (found_application, found_digest, action, capability_digest),
        ) in &inner.unscoped_stream_fences
        {
            if found_tenant != tenant || found_application != application_id {
                continue;
            }
            if semantic_digest
                .as_ref()
                .is_some_and(|existing| existing != found_digest)
            {
                return Err(PersistenceError::Storage(
                    "installed application publication fence is inconsistent".into(),
                ));
            }
            semantic_digest = Some(found_digest.clone());
            let version = inner
                .journals
                .iter()
                .filter_map(|(persistence_id, events)| {
                    parse_persistence_id_parts(persistence_id)
                        .ok()
                        .filter(|(journal_tenant, journal_type, entity_id)| {
                            *journal_tenant == tenant
                                && *journal_type == entity_type
                                && split_scoped_journal_entity_id(entity_id).is_none()
                        })
                        .map(|_| events.len())
                })
                .try_fold(0_u64, |total, count| {
                    u64::try_from(count)
                        .ok()
                        .and_then(|count| total.checked_add(count))
                })
                .ok_or_else(|| {
                    PersistenceError::Storage("global write version exhausted".into())
                })?;
            bindings.insert(
                entity_type.clone(),
                temper_runtime::persistence::schema_deployment::UnscopedStreamPublicationBinding {
                    publication_action: action.clone(),
                    capability_digest: capability_digest.clone(),
                    expected_write_version: version,
                },
            );
        }
        Ok(semantic_digest.map(|semantic_digest| {
            temper_runtime::persistence::schema_deployment::StreamPublicationFence::InstalledApplication {
                application_id: application_id.into(),
                semantic_digest,
                bindings,
            }
        }))
    }

    async fn unscoped_stream_publication_fence_active(
        &self,
        tenant: &str,
        entity_type: &str,
        publication_action: &str,
        capability_digest: &str,
    ) -> Result<bool, PersistenceError> {
        let fault_key = format!("{tenant}:_TemperStreamPublicationFence:{entity_type}");
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        if let Some(remaining) = inner.pending_read_failures.get_mut(&fault_key) {
            *remaining -= 1;
            let cleared = *remaining == 0;
            if cleared {
                inner.pending_read_failures.remove(&fault_key);
            }
            return Err(PersistenceError::Storage(format!(
                "injected read failure for {fault_key}"
            )));
        }
        Ok(inner
            .unscoped_stream_fences
            .get(&(tenant.to_string(), entity_type.to_string()))
            .is_some_and(|(_, _, found_action, found_capability_digest)| {
                found_action == publication_action && found_capability_digest == capability_digest
            }))
    }

    async fn restore_unscoped_stream_publication_fence(
        &self,
        tenant: &str,
        expected_current_semantic_digest: &str,
        fence: &temper_runtime::persistence::schema_deployment::StreamPublicationFence,
    ) -> Result<(), PersistenceError> {
        let temper_runtime::persistence::schema_deployment::StreamPublicationFence::InstalledApplication {
            application_id,
            semantic_digest,
            bindings,
        } = fence
        else {
            return Err(PersistenceError::Storage(
                "task fence cannot restore global stream publications".into(),
            ));
        };
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let current_matches = inner.unscoped_stream_fences.iter().any(
            |((found_tenant, _), (found_application, found_digest, _, _))| {
                found_tenant == tenant
                    && found_application == application_id
                    && found_digest == expected_current_semantic_digest
            },
        );
        if !current_matches {
            return Err(PersistenceError::Storage(
                "installed application publication fence changed concurrently".into(),
            ));
        }
        inner
            .unscoped_stream_fences
            .retain(|(found_tenant, _), (found_application, _, _, _)| {
                found_tenant != tenant || found_application != application_id
            });
        for (entity_type, binding) in bindings {
            inner.unscoped_stream_fences.insert(
                (tenant.into(), entity_type.clone()),
                (
                    application_id.clone(),
                    semantic_digest.clone(),
                    binding.publication_action.clone(),
                    binding.capability_digest.clone(),
                ),
            );
        }
        Ok(())
    }

    async fn list_scoped_entity_ids_page(
        &self,
        tenant: &str,
        entity_type: &str,
        scope: &SchemaScope,
        bundle_digest: &str,
        after_entity_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<String>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let suffix = scoped_journal_pin_suffix(&SchemaExecutionPin {
            scope: scope.clone(),
            bundle_digest: bundle_digest.to_string(),
        });
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(inner
            .journals
            .keys()
            .filter_map(|persistence_id| {
                parse_persistence_id_parts(persistence_id)
                    .ok()
                    .filter(|(found_tenant, found_type, _)| {
                        *found_tenant == tenant && *found_type == entity_type
                    })
                    .and_then(|(_, _, journal_entity_id)| journal_entity_id.strip_suffix(&suffix))
            })
            .filter(|entity_id| after_entity_id.is_none_or(|after| *entity_id > after))
            .take(limit)
            .map(str::to_string)
            .collect())
    }

    async fn scoped_entity_bundle_digests(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        scope: &SchemaScope,
        limit: usize,
    ) -> Result<Vec<String>, PersistenceError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(inner
            .journals
            .keys()
            .filter_map(|persistence_id| {
                parse_persistence_id_parts(persistence_id)
                    .ok()
                    .filter(|(found_tenant, found_type, _)| {
                        *found_tenant == tenant && *found_type == entity_type
                    })
                    .and_then(|(_, _, journal_entity_id)| {
                        split_scoped_journal_entity_id(journal_entity_id)
                    })
                    .filter(|(found_entity_id, pin)| {
                        *found_entity_id == entity_id && &pin.scope == scope
                    })
                    .map(|(_, pin)| pin.bundle_digest)
            })
            .take(limit)
            .collect())
    }

    async fn scoped_bundle_write_version(
        &self,
        tenant: &str,
        scope: &SchemaScope,
        bundle_digest: &str,
    ) -> Result<u64, PersistenceError> {
        let suffix = scoped_journal_pin_suffix(&SchemaExecutionPin {
            scope: scope.clone(),
            bundle_digest: bundle_digest.to_string(),
        });
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner
            .journals
            .iter()
            .filter_map(|(persistence_id, events)| {
                parse_persistence_id_parts(persistence_id)
                    .ok()
                    .filter(|(found_tenant, _, entity_id)| {
                        *found_tenant == tenant && entity_id.ends_with(&suffix)
                    })
                    .map(|_| events.len())
            })
            .try_fold(0_u64, |version, count| {
                version
                    .checked_add(u64::try_from(count).map_err(|_| {
                        PersistenceError::Storage("schema write version exhausted".into())
                    })?)
                    .ok_or_else(|| {
                        PersistenceError::Storage("schema write version exhausted".into())
                    })
            })
    }
}

#[cfg(test)]
mod tests;
