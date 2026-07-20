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
    EntityVectorCandidate, EntityVectorRow, EventStore, JournalRead, PersistenceAppend,
    PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
};
use temper_runtime::tenant::parse_persistence_id_parts;

mod event_store;

fn is_entity_tombstone(event: &PersistenceEnvelope) -> bool {
    match event
        .payload
        .get("to_status")
        .and_then(serde_json::Value::as_str)
    {
        Some(status) => status == "Deleted",
        None => event.event_type == "Deleted",
    }
}

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
    /// Probability of snapshot load failure.
    pub snapshot_load_failure_prob: f64,
}

impl SimFaultConfig {
    /// No fault injection — all operations succeed.
    pub fn none() -> Self {
        Self {
            write_failure_prob: 0.0,
            concurrency_violation_prob: 0.0,
            read_truncation_prob: 0.0,
            snapshot_failure_prob: 0.0,
            snapshot_load_failure_prob: 0.0,
        }
    }

    /// Heavy fault injection for stress testing.
    pub fn heavy() -> Self {
        Self {
            write_failure_prob: 0.05,
            concurrency_violation_prob: 0.02,
            read_truncation_prob: 0.01,
            snapshot_failure_prob: 0.03,
            snapshot_load_failure_prob: 0.03,
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
    /// One-shot atomic-batch append delays keyed by a member persistence ID.
    ///
    /// A batch consumes at most one delay: the first queued delay found while
    /// visiting its inputs in their deterministic caller-provided order. This
    /// lets DST tests hold an already-prepared batch before its OCC check.
    pending_append_batch_delays: BTreeMap<String, VecDeque<Duration>>,
    /// One-shot snapshot-save delays per `persistence_id`.
    ///
    /// Passivation campaigns use these delays to admit actor traffic after an
    /// idle candidate is selected but before its snapshot write completes.
    pending_snapshot_delays: BTreeMap<String, VecDeque<Duration>>,
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
                journals: BTreeMap::new(),
                snapshots: BTreeMap::new(),
                snapshot_history: BTreeMap::new(),
                event_segments: BTreeMap::new(),
                rng: DeterministicRng::new(seed),
                faults,
                pending_concurrency_violations: BTreeMap::new(),
                pending_read_failures: BTreeMap::new(),
                pending_append_delays: BTreeMap::new(),
                pending_append_batch_delays: BTreeMap::new(),
                pending_snapshot_delays: BTreeMap::new(),
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

    /// Return how many deterministic append delays remain queued for an actor.
    ///
    /// Fault campaigns use this to synchronize after an append has consumed
    /// its delay and entered the controlled persistence window.
    pub fn pending_append_delays(&self, persistence_id: &str) -> usize {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .pending_append_delays
            .get(persistence_id)
            .map_or(0, VecDeque::len)
    }

    /// Delay the next atomic batch containing `persistence_id` by `delay`.
    ///
    /// The delay is consumed once. Multiple calls queue multiple delays in
    /// FIFO order.
    pub fn inject_append_batch_delay(&self, persistence_id: &str, delay: Duration) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .pending_append_batch_delays
            .entry(persistence_id.to_string())
            .or_default()
            .push_back(delay);
    }

    /// Return how many deterministic atomic-batch delays remain queued.
    ///
    /// Fault campaigns use this to synchronize after a prepared batch enters
    /// its controlled persistence window.
    pub fn pending_append_batch_delays(&self, persistence_id: &str) -> usize {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .pending_append_batch_delays
            .get(persistence_id)
            .map_or(0, VecDeque::len)
    }

    /// Delay the next snapshot save for `persistence_id` by `delay`.
    ///
    /// The delay is consumed once. Multiple calls queue multiple delays in
    /// FIFO order.
    pub fn inject_snapshot_delay(&self, persistence_id: &str, delay: Duration) {
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .pending_snapshot_delays
            .entry(persistence_id.to_string())
            .or_default()
            .push_back(delay);
    }

    /// Return how many deterministic snapshot delays remain queued.
    pub fn pending_snapshot_delays(&self, persistence_id: &str) -> usize {
        let inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .pending_snapshot_delays
            .get(persistence_id)
            .map_or(0, VecDeque::len)
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

    /// Return one immutable snapshot-history payload for test assertions.
    pub fn snapshot_history_at(&self, persistence_id: &str, sequence_nr: u64) -> Option<Vec<u8>> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner
            .snapshot_history
            .get(persistence_id)
            .and_then(|history| history.get(&sequence_nr))
            .cloned()
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

#[cfg(test)]
mod tests;

#[cfg(test)]
mod tombstone_listing_tests;
