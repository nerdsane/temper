//! In-memory, deterministic event store for simulation testing.
//!
//! `SimEventStore` implements the [`EventStore`] trait using `BTreeMap` journals.
//! All operations resolve immediately and deterministically. Fault injection
//! is controlled by a seeded RNG for reproducible failures.
//!
//! This crate follows the FoundationDB pattern: swap the I/O, keep the code.
//! Server tests route this implementation through the `StorageStack`
//! event-journal capability so production actor code runs unchanged.

mod append;
mod append_batch;
mod fault_injection;
mod key_index;
mod source_fence;

use fault_injection::SimAppendPauseState;
pub use fault_injection::{DeterministicRng, SimAppendPause, SimFaultConfig};

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};
use tokio::sync::Notify;

use temper_runtime::persistence::{
    EntityKeyLookup, EntityVectorCandidate, EntityVectorRow, EventStore, IndexReconciliation,
    JournalBoundary, KeyContractActivation, PersistenceAppend, PersistenceAppendResult,
    PersistenceBatchIdempotency, PersistenceEnvelope, PersistenceError, SnapshotSourceFence,
    is_state_materialization_event_for,
};
use temper_runtime::tenant::parse_persistence_id_parts;

use key_index::{
    KeyContractUse, activate_key_contract_locked, invalidate_coverage_for_snapshot_write_locked,
    invalidate_coverage_for_unreconciled_append_locked, reconcile_key_contract_locked,
};

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
    /// Content-bound claims co-committed with atomic append batches.
    batch_idempotency: BTreeMap<(String, String), String>,
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
    /// Number of upcoming key-contract activation transactions that fail before
    /// mutation. This deterministic fault proves persist-first publication stays
    /// gated across the durable-spec/contract-activation crash boundary.
    pending_key_activation_failures: usize,
    /// One-shot deterministic post-commit append barriers per stream.
    pending_postcommit_append_pauses: BTreeMap<String, VecDeque<SimAppendPauseState>>,
    /// One-shot deterministic pre-commit append barriers per stream.
    pending_append_pauses: BTreeMap<String, VecDeque<SimAppendPauseState>>,
    /// One-shot deterministic pre-commit barriers for atomic append batches.
    pending_batch_pauses: VecDeque<SimAppendPauseState>,
    /// ADR-0153: declared key-index, co-committed with the journal under the same
    /// lock. `(tenant, entity_type, key_name, key_hash) -> (entity_id, sequence_nr)`.
    /// This is the
    /// deterministic reference for the negative-existence access path the real
    /// stores maintain in `entity_key_index`.
    key_index: BTreeMap<(String, String, String, String), (String, u64)>,
    /// ADR-0153/0171 backfill watermark: `(tenant, entity_type) -> key_set` — each
    /// completed type mapped to the versioned declared-key signature covered.
    /// The deterministic reference for the real stores' `key_index_backfill_watermark`
    /// table — gates authoritative keyed absence, and detects a key-set change so a
    /// newly-declared key re-keys instead of being treated as already complete.
    key_index_watermark: BTreeMap<(String, String), String>,
    /// Monotonic key-contract state for ABA-safe watermark publication:
    /// `(tenant, entity_type) -> (versioned signature, revision)`.
    key_index_contract: BTreeMap<(String, String), (String, u64)>,
    /// Latest spec-activated signature. Once present, delayed writers carrying
    /// any other signature are rejected instead of redefining the contract.
    key_index_activated_contract: BTreeMap<(String, String), (String, u64, String)>,
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
                batch_idempotency: BTreeMap::new(),
                snapshots: BTreeMap::new(),
                snapshot_history: BTreeMap::new(),
                event_segments: BTreeMap::new(),
                rng: DeterministicRng::new(seed),
                faults,
                pending_concurrency_violations: BTreeMap::new(),
                pending_read_failures: BTreeMap::new(),
                pending_key_activation_failures: 0,
                pending_postcommit_append_pauses: BTreeMap::new(),
                pending_append_pauses: BTreeMap::new(),
                pending_batch_pauses: VecDeque::new(),
                key_index: BTreeMap::new(),
                key_index_watermark: BTreeMap::new(),
                key_index_contract: BTreeMap::new(),
                key_index_activated_contract: BTreeMap::new(),
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

    /// Fail the next `count` key-contract activation transactions before any
    /// contract, watermark, or ownership row is mutated. `count == 0` clears
    /// the deterministic fault.
    pub fn fail_next_key_activations(&self, count: usize) {
        self.inner
            .lock()
            .expect("SimEventStore lock poisoned") // ci-ok: infallible lock
            .pending_key_activation_failures = count;
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

    /// Pause the next append for `persistence_id` after its journal/index
    /// mutation commits but before the append future acknowledges success.
    /// Tests control the interleaving explicitly through the returned barrier;
    /// no wall-clock timer participates in simulation.
    pub fn inject_postcommit_append_pause(&self, persistence_id: &str) -> SimAppendPause {
        let state = SimAppendPauseState {
            reached: Arc::new(Notify::new()),
            resume: Arc::new(Notify::new()),
        };
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pending_postcommit_append_pauses
            .entry(persistence_id.to_string())
            .or_default()
            .push_back(state.clone());
        SimAppendPause { state }
    }

    /// Pause the next append for `persistence_id` immediately before it takes
    /// the store lock or mutates journal/index state.
    pub fn inject_precommit_append_pause(&self, persistence_id: &str) -> SimAppendPause {
        let state = SimAppendPauseState {
            reached: Arc::new(Notify::new()),
            resume: Arc::new(Notify::new()),
        };
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pending_append_pauses
            .entry(persistence_id.to_string())
            .or_default()
            .push_back(state.clone());
        SimAppendPause { state }
    }

    /// Pause the next atomic append batch before it takes the store lock or
    /// mutates any journal/index state.
    pub fn inject_precommit_batch_pause(&self) -> SimAppendPause {
        let state = SimAppendPauseState {
            reached: Arc::new(Notify::new()),
            resume: Arc::new(Notify::new()),
        };
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .pending_batch_pauses
            .push_back(state.clone());
        SimAppendPause { state }
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
    fn supports_authoritative_key_index(&self) -> bool {
        true
    }

    async fn batch_idempotency_committed(
        &self,
        claim: &PersistenceBatchIdempotency,
    ) -> Result<bool, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned");
        let claim_key = (claim.persistence_id.clone(), claim.idempotency_key.clone());
        let Some(committed_hash) = inner.batch_idempotency.get(&claim_key) else {
            return Ok(false);
        };
        if committed_hash != &claim.intent_hash {
            return Err(PersistenceError::Storage(format!(
                "atomic batch idempotency key '{}' was reused with a different intent",
                claim.idempotency_key
            )));
        }
        Ok(true)
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
            IndexReconciliation::default(),
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
        reconciliation: IndexReconciliation,
    ) -> Result<u64, PersistenceError> {
        self.append_with_index_rows_inner(
            persistence_id,
            expected_sequence,
            events,
            key_rows,
            vector_rows,
            reconciliation,
        )
        .await
    }

    async fn backfill_entity_keys(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        expected_sequence: u64,
        contract_fence: temper_runtime::persistence::KeyIndexBackfillFence<'_>,
        key_rows: &[temper_runtime::persistence::EntityKeyRow],
    ) -> Result<(), PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        key_index::backfill_entity_keys(
            &mut inner,
            tenant,
            entity_type,
            entity_id,
            expected_sequence,
            contract_fence,
            key_rows,
        )
    }

    async fn lookup_by_key(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(
            key_index::lookup(&inner, tenant, entity_type, key_name, key_hash)
                .map(|lookup| lookup.entity_id),
        )
    }

    async fn lookup_by_key_with_sequence(
        &self,
        tenant: &str,
        entity_type: &str,
        key_name: &str,
        key_hash: &str,
    ) -> Result<Option<EntityKeyLookup>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(key_index::lookup(
            &inner,
            tenant,
            entity_type,
            key_name,
            key_hash,
        ))
    }

    async fn mark_key_index_backfilled(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> Result<(), PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        key_index::mark_backfilled(&mut inner, tenant, entity_type, key_set)
    }

    async fn key_index_backfilled_types(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(key_index::backfilled_types(&inner, tenant))
    }

    async fn key_index_activated_contracts(
        &self,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(key_index::activated_contracts(&inner))
    }

    async fn key_index_reconciliation_revision(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<u64, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(key_index::reconciliation_revision(
            &inner,
            tenant,
            entity_type,
        ))
    }

    async fn begin_key_index_backfill(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
    ) -> Result<u64, PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        reconcile_key_contract_locked(
            &mut inner,
            tenant,
            entity_type,
            Some(key_set),
            KeyContractUse::Backfill,
        )
    }

    async fn activate_key_index_contract(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
        purge_existing_rows: bool,
    ) -> Result<u64, PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        if inner.pending_key_activation_failures > 0 {
            inner.pending_key_activation_failures -= 1;
            return Err(PersistenceError::Storage(
                "SimEventStore: injected key activation failure".to_string(),
            ));
        }
        activate_key_contract_locked(
            &mut inner,
            tenant,
            entity_type,
            key_set,
            key_set,
            purge_existing_rows,
        )
    }

    async fn activate_key_index_contracts(
        &self,
        tenant: &str,
        activations: &[KeyContractActivation],
    ) -> Result<BTreeMap<String, u64>, PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        if inner.pending_key_activation_failures > 0 {
            inner.pending_key_activation_failures -= 1;
            return Err(PersistenceError::Storage(
                "SimEventStore: injected key activation failure".to_string(),
            ));
        }
        let mut seen = BTreeSet::new();
        for activation in activations {
            if !seen.insert(activation.entity_type.as_str()) {
                return Err(PersistenceError::Storage(format!(
                    "SimEventStore: duplicate key activation for {tenant}:{}",
                    activation.entity_type
                )));
            }
        }
        let contract_before = inner.key_index_contract.clone();
        let activated_before = inner.key_index_activated_contract.clone();
        let watermark_before = inner.key_index_watermark.clone();
        let rows_before = inner.key_index.clone();
        let mut epochs = BTreeMap::new();
        for activation in activations {
            match activate_key_contract_locked(
                &mut inner,
                tenant,
                &activation.entity_type,
                &activation.key_set,
                &activation.spec_fingerprint,
                activation.purge_existing_rows,
            ) {
                Ok(epoch) => {
                    epochs.insert(activation.entity_type.clone(), epoch);
                }
                Err(error) => {
                    inner.key_index_contract = contract_before;
                    inner.key_index_activated_contract = activated_before;
                    inner.key_index_watermark = watermark_before;
                    inner.key_index = rows_before;
                    return Err(error);
                }
            }
        }
        Ok(epochs)
    }

    async fn mark_key_index_backfilled_if_revision(
        &self,
        tenant: &str,
        entity_type: &str,
        key_set: &str,
        expected_revision: u64,
    ) -> Result<bool, PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(key_index::mark_backfilled_if_revision(
            &mut inner,
            tenant,
            entity_type,
            key_set,
            expected_revision,
        ))
    }

    async fn keyed_entity_ids_for_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(key_index::keyed_entity_ids(&inner, tenant, entity_type))
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
        self.append_batch_inner(appends).await
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

    async fn read_events_page(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        through_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        assert!(limit > 0, "event page limit must be positive");
        assert!(
            through_sequence >= from_sequence,
            "event page boundary must not precede its cursor"
        );
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

        let journal = match inner.journals.get(persistence_id) {
            Some(journal) => journal,
            None => return Ok(Vec::new()),
        };
        let mut events = journal
            .iter()
            .filter(|event| {
                event.sequence_nr > from_sequence && event.sequence_nr <= through_sequence
            })
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

    async fn journal_boundary(
        &self,
        persistence_id: &str,
    ) -> Result<JournalBoundary, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let Some(journal) = inner.journals.get(persistence_id) else {
            return Ok(JournalBoundary::default());
        };
        Ok(JournalBoundary {
            latest_sequence: journal.last().map(|event| event.sequence_nr).unwrap_or(0),
            first_terminal_sequence: journal
                .iter()
                .find(|event| event.transitions_to_deleted())
                .map(|event| event.sequence_nr),
        })
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        self.save_snapshot_locked(
            persistence_id,
            sequence_nr,
            snapshot,
            &SnapshotSourceFence::Unchecked,
            None,
        )
    }

    async fn save_snapshot_if_source(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
        source: &SnapshotSourceFence,
        key_contract: Option<&str>,
    ) -> Result<(), PersistenceError> {
        self.save_snapshot_locked(persistence_id, sequence_nr, snapshot, source, key_contract)
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
        Ok(key_index::live_entity_ids(&inner, tenant, entity_type))
    }

    async fn list_entity_ids_for_key_reconciliation(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(key_index::reconciliation_entity_ids(
            &inner,
            tenant,
            entity_type,
        ))
    }

    async fn list_key_reconciliation_page(
        &self,
        tenant: &str,
        entity_type: &str,
        after_entity_id: Option<&str>,
        through_entity_id: &str,
        limit: usize,
    ) -> Result<Vec<temper_runtime::persistence::KeyReconciliationEntity>, PersistenceError> {
        assert!(limit > 0, "key reconciliation page limit must be positive");
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        let live = key_index::live_entity_ids(&inner, tenant, entity_type)
            .into_iter()
            .collect::<std::collections::BTreeSet<_>>();
        Ok(
            key_index::reconciliation_entity_ids(&inner, tenant, entity_type)
                .into_iter()
                .filter(|entity_id| {
                    after_entity_id.is_none_or(|cursor| entity_id.as_str() > cursor)
                        && entity_id.as_str() <= through_entity_id
                })
                .take(limit)
                .map(
                    |entity_id| temper_runtime::persistence::KeyReconciliationEntity {
                        is_live: live.contains(&entity_id),
                        entity_id,
                    },
                )
                .collect(),
        )
    }

    async fn key_reconciliation_boundary(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Option<String>, PersistenceError> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        Ok(
            key_index::reconciliation_entity_ids(&inner, tenant, entity_type)
                .into_iter()
                .max(),
        )
    }
}

#[cfg(test)]
mod tests;
