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
    CommitRows, EntityVectorCandidate, EntityVectorRow, EventStore, PersistenceAppend,
    PersistenceAppendResult, PersistenceEnvelope, PersistenceError, ScheduledActionRecord,
    StateTimerRecord,
};
use temper_runtime::tenant::parse_persistence_id_parts;

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
    /// How many times `read_events` was called for each id, failed reads
    /// included. See `reads`.
    reads: BTreeMap<String, usize>,
    /// Each entry makes the next N appends that write that id (alone or in a
    /// batch) fail with a storage error, writing nothing. See `fail_next_appends`.
    pending_append_failures: BTreeMap<String, usize>,
    /// Each entry makes the next N single-journal appends to that id write their
    /// events and then answer with a concurrency violation, as a store does when
    /// it retries a write whose first answer was lost. See `lose_next_append_answers`.
    pending_lost_append_answers: BTreeMap<String, usize>,
    /// Each entry makes the next N appends that write that id (alone or in a
    /// batch) write their events and rows and then answer with a storage
    /// error, as a store does when a commit lands but its answer is lost. See
    /// `land_next_appends_then_fail`.
    pending_landed_failures: BTreeMap<String, usize>,
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
    /// Pending scheduled actions, committed with the event that armed them
    /// under the same lock. `(tenant, entity_type, entity_id, arm_id) -> row`.
    scheduled_actions: BTreeMap<(String, String, String, String), ScheduledActionRecord>,
    /// While set, `delete_scheduled_action` never completes, as when the
    /// process stops before it. See `hold_scheduled_action_deletes`.
    scheduled_deletes_held: bool,
    /// Each entry makes `list_scheduled_actions` for that tenant fail with a
    /// storage error on the next N calls. See `fail_next_scheduled_lists`.
    pending_scheduled_list_failures: BTreeMap<String, usize>,
    /// ADR-0155 backfill watermark: `(tenant, entity_type) -> vector_set` — each
    /// completed type mapped to the sorted comma-joined declared vector-path names the
    /// backfill covered. Mirrors `key_index_watermark`.
    vector_index_watermark: BTreeMap<(String, String), String>,
    /// Query-plane rows written with their entity's events, by persistence id.
    projections: BTreeMap<String, entity_rows::SimProjection>,
    /// One state-timer row per entity: `(tenant, entity_type, entity_id)`.
    state_timers: BTreeMap<(String, String, String), StateTimerRecord>,
    /// How many commits wrote each persistence id's journal or rows.
    commits: BTreeMap<String, u64>,
    /// Whether appends are written. See `stop_writes_after_next_append`.
    writes: write_stops::Writes,
    /// The append set to hang. See `hang_next_append`.
    hang: write_hangs::Hang,
    /// Appends refused while the store took no writes, by the journal each
    /// would have written. See `refused_appends`.
    refused_appends: BTreeMap<String, u64>,
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
    fn lock(&self) -> std::sync::MutexGuard<'_, SimEventStoreInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Create a new SimEventStore with the given seed and fault config.
    pub fn new(seed: u64, faults: SimFaultConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SimEventStoreInner {
                journals: BTreeMap::new(),
                snapshots: BTreeMap::new(),
                event_segments: BTreeMap::new(),
                rng: DeterministicRng::new(seed),
                faults,
                pending_concurrency_violations: BTreeMap::new(),
                pending_read_failures: BTreeMap::new(),
                reads: BTreeMap::new(),
                pending_append_failures: BTreeMap::new(),
                pending_lost_append_answers: BTreeMap::new(),
                pending_landed_failures: BTreeMap::new(),
                pending_append_delays: BTreeMap::new(),
                key_index: BTreeMap::new(),
                key_index_watermark: BTreeMap::new(),
                vector_index: BTreeMap::new(),
                scheduled_actions: BTreeMap::new(),
                scheduled_deletes_held: false,
                pending_scheduled_list_failures: BTreeMap::new(),
                vector_index_watermark: BTreeMap::new(),
                projections: BTreeMap::new(),
                state_timers: BTreeMap::new(),
                commits: BTreeMap::new(),
                writes: write_stops::Writes::default(),
                hang: write_hangs::Hang::default(),
                refused_appends: BTreeMap::new(),
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

    /// While `held`, every `delete_scheduled_action` call waits forever and
    /// removes nothing, as when the process stops right after a fire, before
    /// any removal that is not part of the fire's commit. A test sets it, lets
    /// a fire run, stops the process, and clears it before the next boot.
    pub fn hold_scheduled_action_deletes(&self, held: bool) {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner.scheduled_deletes_held = held;
    }

    /// Make the next `count` `list_scheduled_actions` calls for `tenant` fail
    /// with a storage error, then behave normally, as when the store is
    /// briefly unreachable at boot. `count == 0` clears the injection.
    pub fn fail_next_scheduled_lists(&self, tenant: &str, count: usize) {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        if count == 0 {
            inner.pending_scheduled_list_failures.remove(tenant);
        } else {
            inner
                .pending_scheduled_list_failures
                .insert(tenant.to_string(), count);
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

    /// How many times `read_events` was called for `persistence_id`, failed
    /// reads included, so a test can prove a record is not read again.
    pub fn reads(&self, persistence_id: &str) -> usize {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner.reads.get(persistence_id).copied().unwrap_or(0)
    }

    /// Make the next `count` appends that write `persistence_id`, alone or in a
    /// batch, fail with a storage error and write nothing, then behave normally.
    /// Deterministic (unlike `write_failure_prob`) so tests can prove how a
    /// failed write is answered. `count == 0` clears the injection.
    pub fn fail_next_appends(&self, persistence_id: &str, count: usize) {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        if count == 0 {
            inner.pending_append_failures.remove(persistence_id);
        } else {
            inner
                .pending_append_failures
                .insert(persistence_id.to_string(), count);
        }
    }

    /// Return how many injected append failures are still pending for
    /// `persistence_id`. Zero if none are set.
    pub fn pending_append_failures(&self, persistence_id: &str) -> usize {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner
            .pending_append_failures
            .get(persistence_id)
            .copied()
            .unwrap_or(0)
    }

    /// Make the next `count` appends to `persistence_id`, alone or in a batch,
    /// write their events and then answer with a concurrency violation, then
    /// behave normally. This is what a store that retries a write whose answer
    /// was lost reports (the Turso store does): the write landed, and the
    /// retry finds the journal already past its expected sequence. A batch
    /// that holds such a journal lands whole and answers with the violation.
    /// `count == 0` clears the injection.
    pub fn lose_next_append_answers(&self, persistence_id: &str, count: usize) {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        if count == 0 {
            inner.pending_lost_append_answers.remove(persistence_id);
        } else {
            inner
                .pending_lost_append_answers
                .insert(persistence_id.to_string(), count);
        }
    }

    /// Make the next `count` appends that write `persistence_id`, alone or in a
    /// batch, write their events and every row they carry and then answer with
    /// a storage error, then behave normally. This is a commit that landed
    /// while its caller was told it failed: a connection lost after the
    /// commit, or a commit abandoned by a timeout after it was published.
    /// `count == 0` clears the injection.
    pub fn land_next_appends_then_fail(&self, persistence_id: &str, count: usize) {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        if count == 0 {
            inner.pending_landed_failures.remove(persistence_id);
        } else {
            inner
                .pending_landed_failures
                .insert(persistence_id.to_string(), count);
        }
    }

    /// Return how many lost append answers are still pending for
    /// `persistence_id`. Zero if none are set.
    pub fn pending_lost_append_answers(&self, persistence_id: &str) -> usize {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner
            .pending_lost_append_answers
            .get(persistence_id)
            .copied()
            .unwrap_or(0)
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

    /// Whether a fault injected for appends to `persistence_id` (a failure, a
    /// conflict, a lost or failed answer, a delay) has yet to meet one. A
    /// test reads it to know the append it faulted was the one it meant.
    pub fn append_faults_pending(&self, persistence_id: &str) -> bool {
        let inner = self.lock();
        inner.pending_append_failures.contains_key(persistence_id)
            || inner
                .pending_lost_append_answers
                .contains_key(persistence_id)
            || inner.pending_landed_failures.contains_key(persistence_id)
            || inner
                .pending_concurrency_violations
                .contains_key(persistence_id)
            || inner.pending_append_delays.contains_key(persistence_id)
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

    pub fn dump_segments(&self, persistence_id: &str) -> Vec<SimEventSegment> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner
            .event_segments
            .get(persistence_id)
            .cloned()
            .unwrap_or_default()
    }
}

impl SimEventStoreInner {
    /// Consume one injected append failure for `persistence_id`, if one is set.
    fn take_append_failure(&mut self, persistence_id: &str) -> Option<PersistenceError> {
        take_one(&mut self.pending_append_failures, persistence_id).then(|| {
            PersistenceError::Storage(format!(
                "SimEventStore: injected append failure for '{persistence_id}'"
            ))
        })
    }

    /// Consume one lost append answer for `persistence_id`, if one is set.
    fn take_lost_append_answer(&mut self, persistence_id: &str) -> bool {
        take_one(&mut self.pending_lost_append_answers, persistence_id)
    }

    /// Consume one injected journal-read failure for `persistence_id`, if one
    /// is set (see `fail_next_reads`). Every journal read checks it.
    fn take_read_failure(&mut self, persistence_id: &str) -> Option<PersistenceError> {
        take_one(&mut self.pending_read_failures, persistence_id).then(|| {
            PersistenceError::Storage(format!("injected read failure for {persistence_id}"))
        })
    }

    /// Consume one landed-then-failed append for `persistence_id`, if one is set.
    fn take_landed_failure(&mut self, persistence_id: &str) -> Option<PersistenceError> {
        take_one(&mut self.pending_landed_failures, persistence_id).then(|| {
            PersistenceError::Storage(format!(
                "SimEventStore: the append to '{persistence_id}' landed and its answer was lost"
            ))
        })
    }
}

/// Consume one of the injections counted for `persistence_id` in `pending`.
/// Returns whether one was pending.
fn take_one(pending: &mut BTreeMap<String, usize>, persistence_id: &str) -> bool {
    let Some(remaining) = pending.get_mut(persistence_id) else {
        return false;
    };
    *remaining -= 1;
    if *remaining == 0 {
        pending.remove(persistence_id);
    }
    true
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
        self.append_with_rows(
            persistence_id,
            expected_sequence,
            events,
            CommitRows::default(),
        )
        .await
    }

    async fn append_with_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        rows: CommitRows<'_>,
    ) -> Result<u64, PersistenceError> {
        let CommitRows {
            key_rows,
            vector_rows,
            reconcile_vectors,
            scheduled_actions,
            fired_scheduled_action,
            ..
        } = rows;
        self.hang_if_set([persistence_id]).await;
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

        if let Some(error) = inner.refused_write([persistence_id]) {
            return Err(error);
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
        if let Some(error) = inner.take_append_failure(persistence_id) {
            return Err(error);
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

        // The scheduled actions the transition armed, under the SAME lock: they
        // exist exactly when the event does. A repeated arm keeps its row.
        for action in scheduled_actions {
            inner
                .scheduled_actions
                .entry((
                    action.tenant.clone(),
                    action.entity_type.clone(),
                    action.entity_id.clone(),
                    action.arm_id.clone(),
                ))
                .or_insert_with(|| action.clone());
        }
        // The scheduled action the transition fired goes in the same commit.
        if let Some(arm_id) = fired_scheduled_action
            && let Ok((tenant, entity_type, entity_id)) = parse_persistence_id_parts(persistence_id)
        {
            inner.scheduled_actions.remove(&(
                tenant.to_string(),
                entity_type.to_string(),
                entity_id.to_string(),
                arm_id.to_string(),
            ));
        }

        // The entity's query-plane and timer rows, in the same commit.
        inner.commit_entity_rows(persistence_id, new_seq, &rows);
        inner.wrote(persistence_id);

        // The write landed; a lost answer reaches the caller as the retry's
        // concurrency violation (see `lose_next_append_answers`).
        if inner.take_lost_append_answer(persistence_id) {
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: new_seq,
            });
        }
        if let Some(error) = inner.take_landed_failure(persistence_id) {
            return Err(error);
        }

        Ok(new_seq)
    }

    fn commits_entity_rows(&self) -> bool {
        true
    }

    async fn list_scheduled_actions(
        &self,
        tenant: &str,
    ) -> Result<Vec<ScheduledActionRecord>, PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        if let Some(left) = inner.pending_scheduled_list_failures.get_mut(tenant) {
            *left -= 1;
            if *left == 0 {
                inner.pending_scheduled_list_failures.remove(tenant);
            }
            return Err(PersistenceError::Storage(
                "simulated scheduled-action list failure".to_string(),
            ));
        }
        Ok(inner
            .scheduled_actions
            .values()
            .filter(|action| action.tenant == tenant)
            .cloned()
            .collect())
    }

    async fn delete_scheduled_action(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        let held = self
            .inner
            .lock()
            .expect("SimEventStore lock poisoned") // ci-ok: infallible lock
            .scheduled_deletes_held;
        if held {
            std::future::pending::<()>().await;
        }
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner.scheduled_actions.remove(&(
            tenant.to_string(),
            entity_type.to_string(),
            entity_id.to_string(),
            arm_id.to_string(),
        ));
        Ok(())
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
        self.hang_if_set(appends.iter().map(|append| append.persistence_id.as_str()))
            .await;

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
        if let Some(error) =
            inner.refused_write(appends.iter().map(|append| append.persistence_id.as_str()))
        {
            return Err(error);
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
        // Each journal in the batch uses up one of its own injected failures.
        let mut failure = None;
        for append in appends {
            let failed = inner.take_append_failure(&append.persistence_id);
            failure = failure.or(failed);
        }
        if let Some(error) = failure {
            return Err(error);
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
            // The record's rows commit with its events, as in one append.
            inner.commit_entity_rows(&append.persistence_id, new_seq, &append.commit_rows());
            results.push(PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr: new_seq,
            });
        }
        for append in appends {
            inner.wrote(&append.persistence_id);
        }

        // The batch landed; a lost answer for any of its journals reaches the
        // caller as the retry's concurrency violation (see
        // `lose_next_append_answers`).
        let mut lost = None;
        for (append, result) in appends.iter().zip(&results) {
            if inner.take_lost_append_answer(&append.persistence_id) {
                lost = lost.or(Some(PersistenceError::ConcurrencyViolation {
                    expected: append.expected_sequence,
                    actual: result.sequence_nr,
                }));
            }
        }
        if let Some(error) = lost {
            return Err(error);
        }
        let mut landed_failure = None;
        for append in appends {
            let failed = inner.take_landed_failure(&append.persistence_id);
            landed_failure = landed_failure.or(failed);
        }
        if let Some(error) = landed_failure {
            return Err(error);
        }

        Ok(results)
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        *inner.reads.entry(persistence_id.to_string()).or_default() += 1;

        if let Some(error) = inner.take_read_failure(persistence_id) {
            return Err(error);
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

    /// The newest `limit` events below `before_sequence`, ascending. Honors
    /// `fail_next_reads`; read truncation models a replay read and is not
    /// applied to a page.
    async fn read_events_page(
        &self,
        persistence_id: &str,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        if let Some(error) = inner.take_read_failure(persistence_id) {
            return Err(error);
        }
        let Some(journal) = inner.journals.get(persistence_id) else {
            return Ok(Vec::new());
        };
        let mut page: Vec<PersistenceEnvelope> = journal
            .iter()
            .rev()
            .filter(|e| before_sequence.is_none_or(|before| e.sequence_nr < before))
            .take(limit)
            .cloned()
            .collect();
        page.reverse();
        Ok(page)
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

    async fn latest_sequence_nr(&self, persistence_id: &str) -> Result<u64, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let journal = inner
            .journals
            .get(persistence_id)
            .and_then(|j| j.last())
            .map_or(0, |e| e.sequence_nr);
        let snapshot = inner
            .snapshots
            .get(persistence_id)
            .map_or(0, |(seq, _)| *seq);
        Ok(journal.max(snapshot))
    }

    async fn journal_heads(
        &self,
        tenant: &str,
    ) -> Result<Vec<temper_runtime::persistence::JournalHead>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let mut heads = BTreeMap::new();
        for (pid, journal) in &inner.journals {
            if let Ok((t, ty, id)) = parse_persistence_id_parts(pid)
                && t == tenant
            {
                heads.insert(
                    (ty.to_string(), id.to_string()),
                    (
                        journal.last().map_or(0, |e| e.sequence_nr),
                        journal.iter().any(|e| e.is_tombstone()),
                    ),
                );
            }
        }
        for (pid, (seq, _)) in &inner.snapshots {
            if let Ok((t, ty, id)) = parse_persistence_id_parts(pid)
                && t == tenant
            {
                let head = heads
                    .entry((ty.to_string(), id.to_string()))
                    .or_insert((0, false));
                head.0 = head.0.max(*seq);
            }
        }
        Ok(heads
            .into_iter()
            .map(|((entity_type, entity_id), (sequence_nr, deleted))| {
                temper_runtime::persistence::JournalHead {
                    entity_type,
                    entity_id,
                    sequence_nr,
                    deleted,
                }
            })
            .collect())
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
mod tests;

mod entity_rows;
pub use entity_rows::SimProjection;

mod write_hangs;
mod write_stops;
