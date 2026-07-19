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
    PersistenceEnvelope, PersistenceError, ProjectionReconciliationFence,
};
use temper_runtime::tenant::parse_persistence_id_parts;
use tokio::sync::{Barrier, RwLock as AsyncRwLock};

type ProjectionPartition = (String, String);
type ProjectionFence = Arc<AsyncRwLock<()>>;
type ProjectionFenceMap = BTreeMap<ProjectionPartition, ProjectionFence>;

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
    /// Probability of detecting an incomplete journal read on `read_events()`.
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
    /// Per-type projection fences shared by every clone of this deterministic
    /// store. Reconciliation takes the write side; live projection writes take
    /// the read side. The map is only held long enough to clone a fence, never
    /// across an await.
    projection_fences: Arc<Mutex<ProjectionFenceMap>>,
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
    /// One-shot failures for durable key-watermark reads, keyed by tenant.
    /// This proves callers distinguish an unreadable watermark from an absent
    /// watermark before beginning destructive reconciliation.
    pending_key_watermark_read_failures: BTreeMap<String, usize>,
    /// One-shot failures for durable vector-watermark reads, keyed by tenant.
    /// Mirrors `pending_key_watermark_read_failures` for exact vector repair.
    pending_vector_watermark_read_failures: BTreeMap<String, usize>,
    /// One-shot append delays per `persistence_id`.
    ///
    /// Used by dispatch retry tests to deterministically model "the actor
    /// persisted the transition, but the caller's ask timeout expired before
    /// the reply arrived".
    pending_append_delays: BTreeMap<String, VecDeque<Duration>>,
    /// One-shot append gates used to deterministically orchestrate in-flight
    /// journal writes against hot-deploy generation changes.
    pending_append_gates: BTreeMap<String, VecDeque<SimAppendGate>>,
    /// ADR-0153: declared key-index, co-committed with the journal under the same
    /// lock. `(tenant, entity_type, key_name, key_hash) -> entity_id`. This is the
    /// deterministic reference for the negative-existence access path the real
    /// stores maintain in `entity_key_index`.
    key_index: BTreeMap<(String, String, String, String), String>,
    /// ADR-0153 backfill watermark: `(tenant, entity_type) -> key_set` — each completed
    /// type mapped to the versioned declared-key signature the backfill covered.
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
    /// completed type mapped to the versioned declared-vector signature the backfill
    /// covered. Mirrors `key_index_watermark`.
    vector_index_watermark: BTreeMap<(String, String), String>,
}

fn journal_head(inner: &SimEventStoreInner, persistence_id: &str) -> u64 {
    inner
        .journals
        .get(persistence_id)
        .and_then(|journal| journal.last())
        .map(|event| event.sequence_nr)
        .unwrap_or(0)
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

/// A deterministic one-shot gate for an injected Sim event-store append.
///
/// The append first rendezvous with [`Self::wait_until_blocked`], then remains
/// paused until the controller calls [`Self::release`]. This avoids timing-based
/// sleeps in durability race regressions.
#[derive(Clone, Debug)]
pub struct SimAppendGate {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

impl SimAppendGate {
    /// Wait until the injected append has reached its controlled pause point.
    pub async fn wait_until_blocked(&self) {
        self.entered.wait().await;
    }

    /// Release the blocked append.
    pub async fn release(&self) {
        self.release.wait().await;
    }
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
                pending_key_watermark_read_failures: BTreeMap::new(),
                pending_vector_watermark_read_failures: BTreeMap::new(),
                pending_append_delays: BTreeMap::new(),
                pending_append_gates: BTreeMap::new(),
                key_index: BTreeMap::new(),
                key_index_watermark: BTreeMap::new(),
                vector_index: BTreeMap::new(),
                vector_index_watermark: BTreeMap::new(),
            })),
            projection_fences: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Inject exactly `count` deterministic `ConcurrencyViolation` errors on
    /// the next `count` `append` calls for `persistence_id`, then behave
    /// normally.
    ///
    /// Use this for retry-path tests where the probabilistic fault injection
    /// in `SimFaultConfig` would be flaky. Each injected violation reports the
    /// current durable journal head as `actual`, matching a real store's
    /// optimistic-concurrency contract.
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

    /// Make the next `count` durable key-watermark reads for `tenant` fail,
    /// then behave normally. `count == 0` clears the injection.
    pub fn fail_next_key_watermark_reads(&self, tenant: &str, count: usize) {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        if count == 0 {
            inner.pending_key_watermark_read_failures.remove(tenant);
        } else {
            inner
                .pending_key_watermark_read_failures
                .insert(tenant.to_string(), count);
        }
    }

    /// Make the next `count` durable vector-watermark reads for `tenant` fail,
    /// then behave normally. `count == 0` clears the injection.
    pub fn fail_next_vector_watermark_reads(&self, tenant: &str, count: usize) {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        if count == 0 {
            inner.pending_vector_watermark_read_failures.remove(tenant);
        } else {
            inner
                .pending_vector_watermark_read_failures
                .insert(tenant.to_string(), count);
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

    /// Pause the next append for `persistence_id` at a deterministic rendezvous.
    ///
    /// Multiple calls queue independent gates in FIFO order.
    pub fn inject_append_gate(&self, persistence_id: &str) -> SimAppendGate {
        let gate = SimAppendGate {
            entered: Arc::new(Barrier::new(2)),
            release: Arc::new(Barrier::new(2)),
        };
        let mut inner = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        inner
            .pending_append_gates
            .entry(persistence_id.to_string())
            .or_default()
            .push_back(gate.clone());
        gate
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

    fn projection_fence(&self, tenant: &str, entity_type: &str) -> Arc<AsyncRwLock<()>> {
        let mut fences = self
            .projection_fences
            .lock()
            .expect("SimEventStore projection-fence lock poisoned"); // ci-ok: infallible lock
        fences
            .entry((tenant.to_string(), entity_type.to_string()))
            .or_insert_with(|| Arc::new(AsyncRwLock::new(())))
            .clone()
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

fn advance_event_segment(
    inner: &mut SimEventStoreInner,
    persistence_id: &str,
    expected_sequence: u64,
    new_sequence: u64,
) {
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
    if segments
        .last()
        .map(|segment| segment.sealed)
        .unwrap_or(true)
    {
        let next_index = segments
            .last()
            .map(|segment| segment.segment_index + 1)
            .unwrap_or(0);
        segments.push(SimEventSegment {
            segment_index: next_index,
            start_sequence_nr: (expected_sequence + 1).max(1),
            end_sequence_nr: None,
            snapshot_sequence: None,
            event_count: 0,
            sealed: false,
        });
    }
    if new_sequence > expected_sequence {
        let active_segment = segments
            .last_mut()
            .expect("segments must contain an active segment");
        active_segment.end_sequence_nr = Some(new_sequence);
        active_segment.event_count = new_sequence
            .saturating_sub(active_segment.start_sequence_nr)
            .saturating_add(1);
    }
}

impl EventStore for SimEventStore {
    fn has_authoritative_key_index(&self) -> bool {
        true
    }

    fn has_durable_vector_backfill_watermark(&self) -> bool {
        true
    }

    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.append_with_index_rows(
            persistence_id,
            expected_sequence,
            events,
            &[],
            &[],
            temper_runtime::persistence::IndexReconciliation::default(),
        )
        .await
    }

    async fn append_with_index_rows(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
        vector_rows: &[EntityVectorRow],
        reconciliation: temper_runtime::persistence::IndexReconciliation,
    ) -> Result<u64, PersistenceError> {
        let (append_delay, append_gate) = {
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
            let gate = inner
                .pending_append_gates
                .get_mut(persistence_id)
                .and_then(VecDeque::pop_front);
            if inner
                .pending_append_gates
                .get(persistence_id)
                .is_some_and(VecDeque::is_empty)
            {
                inner.pending_append_gates.remove(persistence_id);
            }
            (delay, gate)
        };
        if let Some(delay) = append_delay
            && !delay.is_zero()
        {
            tokio::time::sleep(delay).await;
        }
        if let Some(gate) = append_gate {
            gate.entered.wait().await;
            gate.release.wait().await;
        }

        // A live write that maintains either projection family holds the shared
        // side until its journal + derived rows are atomically visible. A
        // type-wide reconciliation holds the write side, so neither can overwrite
        // state derived by the other from an earlier journal head.
        let _projection_guard = if reconciliation.keys || reconciliation.vectors {
            let (tenant, entity_type, _) =
                parse_persistence_id_parts(persistence_id).map_err(PersistenceError::Storage)?;
            Some(
                self.projection_fence(tenant, entity_type)
                    .read_owned()
                    .await,
            )
        } else {
            None
        };

        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        // Deterministic one-shot injection (see `inject_concurrency_violations`).
        // Consumes one counter per call; falls back to normal flow once drained.
        //
        // The reported `actual` is the current durable journal head, matching
        // the optimistic-concurrency contract even for an injected conflict.
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
                actual: journal_head(&inner, persistence_id),
            });
        }

        // Fault injection: spurious concurrency violation (probabilistic).
        let cv_prob = inner.faults.concurrency_violation_prob;
        if inner.rng.chance(cv_prob) {
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: journal_head(&inner, persistence_id),
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

        advance_event_segment(&mut inner, persistence_id, expected_sequence, new_seq);

        // ADR-0153: co-commit the declared key-index rows under the SAME lock as
        // the journal write above (uniqueness was validated before the journal, so
        // this only mutates — never fails). A keyed read is therefore consistent
        // with the journal: the negative-existence access path.
        if reconciliation.keys || !key_rows.is_empty() {
            let mut parts = persistence_id.splitn(3, ':');
            let tenant = parts.next().unwrap_or("");
            let entity_type = parts.next().unwrap_or("");
            let entity_id = parts.next().unwrap_or("");
            if reconciliation.keys {
                inner.key_index.retain(|(t, et, _, _), eid| {
                    !(t.as_str() == tenant
                        && et.as_str() == entity_type
                        && eid.as_str() == entity_id)
                });
            }
            for row in key_rows {
                if !reconciliation.keys {
                    // Legacy partial-key callers replace only the provided key name.
                    inner.key_index.retain(|(t, et, kn, _), eid| {
                        !(t.as_str() == tenant
                            && et.as_str() == entity_type
                            && kn.as_str() == row.key_name.as_str()
                            && eid.as_str() == entity_id)
                    });
                }
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
        if reconciliation.vectors {
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

    async fn acquire_projection_reconciliation_fence(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<ProjectionReconciliationFence, PersistenceError> {
        let guard = self
            .projection_fence(tenant, entity_type)
            .write_owned()
            .await;
        Ok(ProjectionReconciliationFence::new(guard))
    }

    async fn acquire_projection_read_fence(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<ProjectionReconciliationFence, PersistenceError> {
        let guard = self
            .projection_fence(tenant, entity_type)
            .read_owned()
            .await;
        Ok(ProjectionReconciliationFence::new(guard))
    }

    async fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
    ) -> Result<(), PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        // Resolve conflicts against other entities before replacing this entity's
        // complete row set. A duplicate live key is an unresolved data invariant:
        // return an error so the caller leaves the type unwatermarked and scan-safe.
        let mut desired = Vec::with_capacity(key_rows.len());
        for row in key_rows {
            let slot = (
                tenant.to_string(),
                entity_type.to_string(),
                row.key_name.clone(),
                row.key_hash.clone(),
            );
            match inner.key_index.get(&slot) {
                Some(existing) if existing.as_str() != entity_id => {
                    return Err(PersistenceError::Storage(format!(
                        "duplicate declared key '{}' for {entity_type}: held by {existing}",
                        row.key_name
                    )));
                }
                _ => desired.push(slot),
            }
        }
        // Exact reconciliation: purge every prior key name/hash for this entity,
        // including when the current row set is empty (deleted/phantom/unkeyable).
        inner.key_index.retain(|(t, et, _, _), eid| {
            !(t.as_str() == tenant && et.as_str() == entity_type && eid.as_str() == entity_id)
        });
        for slot in desired {
            inner.key_index.insert(slot, entity_id.to_string());
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
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let pending = inner
            .pending_key_watermark_read_failures
            .get(tenant)
            .copied()
            .unwrap_or(0);
        if pending > 0 {
            if pending == 1 {
                inner.pending_key_watermark_read_failures.remove(tenant);
            } else {
                inner
                    .pending_key_watermark_read_failures
                    .insert(tenant.to_string(), pending - 1);
            }
            return Err(PersistenceError::Storage(
                "SimEventStore: injected key watermark read failure".to_string(),
            ));
        }
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
        _source_sequence: u64,
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
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let pending = inner
            .pending_vector_watermark_read_failures
            .get(tenant)
            .copied()
            .unwrap_or(0);
        if pending > 0 {
            if pending == 1 {
                inner.pending_vector_watermark_read_failures.remove(tenant);
            } else {
                inner
                    .pending_vector_watermark_read_failures
                    .insert(tenant.to_string(), pending - 1);
            }
            return Err(PersistenceError::Storage(
                "SimEventStore: injected vector watermark read failure".to_string(),
            ));
        }
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

        let mut seen = BTreeSet::new();
        let mut lock_partitions = BTreeSet::new();
        let mut reconciled_key_entities = BTreeSet::new();
        for append in appends {
            if !seen.insert(append.persistence_id.as_str()) {
                return Err(PersistenceError::Storage(format!(
                    "SimEventStore: duplicate persistence_id '{}' in append_batch",
                    append.persistence_id
                )));
            }
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            if append.reconciliation.keys || append.reconciliation.vectors {
                lock_partitions.insert((tenant.to_string(), entity_type.to_string()));
            }
            if append.reconciliation.keys {
                reconciled_key_entities.insert((
                    tenant.to_string(),
                    entity_type.to_string(),
                    entity_id.to_string(),
                ));
            }
        }
        let mut _projection_guards = Vec::with_capacity(lock_partitions.len());
        for (tenant, entity_type) in lock_partitions {
            _projection_guards.push(
                self.projection_fence(&tenant, &entity_type)
                    .read_owned()
                    .await,
            );
        }

        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

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
                    actual: journal_head(&inner, &append.persistence_id),
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
                actual: journal_head(&inner, &first.persistence_id),
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

        // Validate the batch's final key claims before any journal mutation. Rows
        // owned by another entity being exactly reconciled in this same atomic batch
        // are releasable; all other holders remain conflicts. Desired claims within
        // the batch must also be unique.
        let mut desired_key_claims = BTreeMap::new();
        for append in appends {
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
                if let Some(existing) = desired_key_claims.insert(slot.clone(), entity_id)
                    && existing != entity_id
                {
                    return Err(PersistenceError::Storage(format!(
                        "duplicate declared key '{}' inside atomic batch",
                        row.key_name
                    )));
                }
                if let Some(existing) = inner.key_index.get(&slot)
                    && existing.as_str() != entity_id
                    && !reconciled_key_entities.contains(&(
                        tenant.to_string(),
                        entity_type.to_string(),
                        existing.clone(),
                    ))
                {
                    return Err(PersistenceError::Storage(format!(
                        "duplicate declared key '{}' for {entity_type}: held by {existing}",
                        row.key_name
                    )));
                }
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
            advance_event_segment(
                &mut inner,
                &append.persistence_id,
                append.expected_sequence,
                new_seq,
            );
            results.push(PersistenceAppendResult {
                persistence_id: append.persistence_id.clone(),
                sequence_nr: new_seq,
            });
        }

        // Purge every exact row set before inserting any replacements, allowing
        // deterministic key transfers/swaps between entities in the same batch.
        for append in appends {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            if append.reconciliation.keys {
                inner.key_index.retain(|(t, et, _, _), holder| {
                    !(t == tenant && et == entity_type && holder == entity_id)
                });
            }
            if append.reconciliation.vectors {
                inner.vector_index.retain(|(t, et, _, _, indexed_id), _| {
                    !(t == tenant && et == entity_type && indexed_id == entity_id)
                });
            }
        }
        for append in appends {
            let (tenant, entity_type, entity_id) =
                parse_persistence_id_parts(&append.persistence_id)
                    .map_err(PersistenceError::Storage)?;
            for row in &append.key_rows {
                if !append.reconciliation.keys {
                    inner.key_index.retain(|(t, et, key_name, _), holder| {
                        !(t == tenant
                            && et == entity_type
                            && key_name == &row.key_name
                            && holder == entity_id)
                    });
                }
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
            if append.reconciliation.vectors {
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

        let events: Vec<PersistenceEnvelope> = journal
            .iter()
            .filter(|e| e.sequence_nr > from_sequence)
            .cloned()
            .collect();

        // Fault injection: model a backend detecting that it received only a
        // proper prefix. EventStore requires a complete tail or an error, so a
        // detected truncation must never be returned as successful history.
        let rt_prob = inner.faults.read_truncation_prob;
        if events.len() > 1 && inner.rng.chance(rt_prob) {
            let truncate_at = (inner.rng.next_u64() as usize) % events.len();
            let prefix_len = truncate_at.max(1);
            return Err(PersistenceError::Storage(format!(
                "SimEventStore: incomplete journal read for {persistence_id} \
                 ({prefix_len} of {} tail events)",
                events.len()
            )));
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
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod projection_fence_tests;

#[cfg(test)]
mod batch_projection_tests;
