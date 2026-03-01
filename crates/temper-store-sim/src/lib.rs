//! In-memory, deterministic event store for simulation testing.
//!
//! `SimEventStore` implements the [`EventStore`] trait using `BTreeMap` journals.
//! All operations resolve immediately and deterministically. Fault injection
//! is controlled by a seeded RNG for reproducible failures.
//!
//! This crate follows the FoundationDB pattern: swap the I/O, keep the code.
//! The same `ServerEventStore` dispatch enum adds a `Sim` variant that routes
//! to this implementation. Production code runs unchanged.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use temper_runtime::persistence::{EventStore, PersistenceEnvelope, PersistenceError};
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
    /// Fault injection RNG.
    rng: DeterministicRng,
    /// Fault injection configuration.
    faults: SimFaultConfig,
}

impl SimEventStore {
    /// Create a new SimEventStore with the given seed and fault config.
    pub fn new(seed: u64, faults: SimFaultConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(SimEventStoreInner {
                journals: BTreeMap::new(),
                snapshots: BTreeMap::new(),
                rng: DeterministicRng::new(seed),
                faults,
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

    /// Dump all events for a persistence_id (for test assertions).
    pub fn dump_journal(&self, persistence_id: &str) -> Vec<PersistenceEnvelope> {
        let inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock
        inner
            .journals
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
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

        // Fault injection: spurious concurrency violation.
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

        let journal = inner
            .journals
            .entry(persistence_id.to_string())
            .or_default();

        // Check optimistic concurrency.
        let current_seq = journal.last().map(|e| e.sequence_nr).unwrap_or(0);
        if current_seq != expected_sequence {
            return Err(PersistenceError::ConcurrencyViolation {
                expected: expected_sequence,
                actual: current_seq,
            });
        }

        let mut new_seq = expected_sequence;
        for event in events {
            new_seq += 1;
            // Store with correct sequence number (ignore the one in the envelope,
            // use monotonic counter like the real stores do).
            let mut stored = event.clone();
            stored.sequence_nr = new_seq;
            journal.push(stored);
        }

        Ok(new_seq)
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        let mut inner = self.inner.lock().expect("SimEventStore lock poisoned"); // ci-ok: infallible lock

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
            if let Ok((t, entity_type, entity_id)) =
                parse_persistence_id_parts(persistence_id)
            {
                if t == tenant {
                    let key = (entity_type.to_string(), entity_id.to_string());
                    if seen.insert(key.clone()) {
                        result.push(key);
                    }
                }
            }
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_runtime::persistence::EventMetadata;

    fn test_envelope(seq: u64, event_type: &str) -> PersistenceEnvelope {
        PersistenceEnvelope {
            sequence_nr: seq,
            event_type: event_type.to_string(),
            payload: serde_json::json!({"test": true}),
            metadata: EventMetadata {
                event_id: uuid::Uuid::nil(),
                causation_id: uuid::Uuid::nil(),
                correlation_id: uuid::Uuid::nil(),
                timestamp: chrono::DateTime::UNIX_EPOCH,
                actor_id: "test".to_string(),
            },
        }
    }

    #[tokio::test]
    async fn append_and_read_roundtrip() {
        let store = SimEventStore::no_faults(42);
        let pid = "default:Order:ord-1";

        let new_seq = store
            .append(pid, 0, &[test_envelope(0, "Created")])
            .await
            .unwrap();
        assert_eq!(new_seq, 1);

        let events = store.read_events(pid, 0).await.unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].sequence_nr, 1);
        assert_eq!(events[0].event_type, "Created");
    }

    #[tokio::test]
    async fn append_multiple_events() {
        let store = SimEventStore::no_faults(42);
        let pid = "default:Order:ord-2";

        let seq = store
            .append(
                pid,
                0,
                &[test_envelope(0, "Created"), test_envelope(0, "Submitted")],
            )
            .await
            .unwrap();
        assert_eq!(seq, 2);

        let events = store.read_events(pid, 0).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence_nr, 1);
        assert_eq!(events[1].sequence_nr, 2);
    }

    #[tokio::test]
    async fn concurrency_violation_on_wrong_sequence() {
        let store = SimEventStore::no_faults(42);
        let pid = "default:Order:ord-3";

        store
            .append(pid, 0, &[test_envelope(0, "Created")])
            .await
            .unwrap();

        let err = store
            .append(pid, 0, &[test_envelope(0, "Duplicate")])
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            PersistenceError::ConcurrencyViolation {
                expected: 0,
                actual: 1
            }
        ));
    }

    #[tokio::test]
    async fn snapshot_save_and_load() {
        let store = SimEventStore::no_faults(42);
        let pid = "default:Order:ord-4";

        store
            .save_snapshot(pid, 5, b"state-data")
            .await
            .unwrap();

        let snap = store.load_snapshot(pid).await.unwrap();
        assert_eq!(snap, Some((5, b"state-data".to_vec())));
    }

    #[tokio::test]
    async fn load_snapshot_returns_none_when_empty() {
        let store = SimEventStore::no_faults(42);
        let snap = store
            .load_snapshot("default:Order:nonexistent")
            .await
            .unwrap();
        assert_eq!(snap, None);
    }

    #[tokio::test]
    async fn list_entity_ids_filters_by_tenant() {
        let store = SimEventStore::no_faults(42);

        store
            .append(
                "alpha:Order:ord-1",
                0,
                &[test_envelope(0, "Created")],
            )
            .await
            .unwrap();
        store
            .append(
                "alpha:Task:task-1",
                0,
                &[test_envelope(0, "Created")],
            )
            .await
            .unwrap();
        store
            .append(
                "beta:Order:ord-9",
                0,
                &[test_envelope(0, "Created")],
            )
            .await
            .unwrap();

        let mut alpha = store.list_entity_ids("alpha").await.unwrap();
        alpha.sort();
        assert_eq!(
            alpha,
            vec![
                ("Order".to_string(), "ord-1".to_string()),
                ("Task".to_string(), "task-1".to_string()),
            ]
        );

        let beta = store.list_entity_ids("beta").await.unwrap();
        assert_eq!(
            beta,
            vec![("Order".to_string(), "ord-9".to_string())]
        );
    }

    #[tokio::test]
    async fn read_events_from_sequence() {
        let store = SimEventStore::no_faults(42);
        let pid = "default:Order:ord-5";

        store
            .append(pid, 0, &[test_envelope(0, "A"), test_envelope(0, "B")])
            .await
            .unwrap();
        store
            .append(pid, 2, &[test_envelope(0, "C")])
            .await
            .unwrap();

        // Read from sequence 1 — should skip event at seq 1
        let events = store.read_events(pid, 1).await.unwrap();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].sequence_nr, 2);
        assert_eq!(events[1].sequence_nr, 3);
    }

    #[tokio::test]
    async fn deterministic_across_seeds() {
        // Same seed → same behavior (with no faults, behavior is trivially the same)
        for seed in [42, 123, 999] {
            let store = SimEventStore::no_faults(seed);
            let pid = "default:Order:det-1";

            let seq = store
                .append(pid, 0, &[test_envelope(0, "Created")])
                .await
                .unwrap();
            assert_eq!(seq, 1);

            let events = store.read_events(pid, 0).await.unwrap();
            assert_eq!(events.len(), 1);
        }
    }

    #[tokio::test]
    async fn fault_injection_produces_errors() {
        let faults = SimFaultConfig {
            write_failure_prob: 1.0, // always fail
            concurrency_violation_prob: 0.0,
            read_truncation_prob: 0.0,
            snapshot_failure_prob: 0.0,
        };
        let store = SimEventStore::new(42, faults);
        let pid = "default:Order:fault-1";

        let err = store
            .append(pid, 0, &[test_envelope(0, "Created")])
            .await;
        assert!(err.is_err());
    }
}
