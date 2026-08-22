//! Bounded, coalescing snapshot writer for entity actors.
//!
//! The event journal append remains synchronous. Snapshot rows are derived
//! recovery accelerators, so this queue moves their writes off the actor hot
//! path while preserving the latest accepted sequence per persistence stream.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tracing::Instrument;

use crate::storage::BoxedEventStore;

const DEFAULT_SNAPSHOT_QUEUE_CAPACITY: usize = 20_000;
const DEFAULT_SNAPSHOT_DRAIN_BATCH: usize = 256;

#[derive(Clone, Debug)]
struct QueuedSnapshotWrite {
    persistence_id: String,
    sequence_nr: u64,
    snapshot: Vec<u8>,
    enqueued_at: Instant,
}

#[derive(Debug, Default)]
struct PendingSnapshotWrites {
    writes: BTreeMap<String, QueuedSnapshotWrite>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum SnapshotEnqueueOutcome {
    Enqueued,
    Coalesced,
    StaleSkipped,
    Full,
}

/// Background snapshot writer shared by all entity actors for one storage stack.
pub(crate) struct SnapshotWriteQueue {
    store: BoxedEventStore,
    pending: Arc<Mutex<PendingSnapshotWrites>>,
    applied_sequences: Arc<Mutex<BTreeMap<String, u64>>>,
    notify: Arc<Notify>,
    capacity: usize,
    drain_batch: usize,
}

impl SnapshotWriteQueue {
    pub(crate) fn start(store: BoxedEventStore) -> Arc<Self> {
        let queue = Arc::new(Self {
            store,
            pending: Arc::new(Mutex::new(PendingSnapshotWrites::default())),
            applied_sequences: Arc::new(Mutex::new(BTreeMap::new())),
            notify: Arc::new(Notify::new()),
            capacity: snapshot_queue_capacity(),
            drain_batch: snapshot_drain_batch(),
        });
        queue.spawn_worker();
        queue
    }

    #[cfg(test)]
    fn new_for_test(store: BoxedEventStore, capacity: usize, drain_batch: usize) -> Self {
        Self {
            store,
            pending: Arc::new(Mutex::new(PendingSnapshotWrites::default())),
            applied_sequences: Arc::new(Mutex::new(BTreeMap::new())),
            notify: Arc::new(Notify::new()),
            capacity,
            drain_batch,
        }
    }

    pub(crate) fn enqueue(
        &self,
        persistence_id: String,
        sequence_nr: u64,
        snapshot: Vec<u8>,
    ) -> SnapshotEnqueueOutcome {
        let mut pending = self.pending.lock().expect("snapshot queue mutex poisoned");
        let existing = pending.writes.get(&persistence_id);
        if existing.is_some_and(|existing| existing.sequence_nr >= sequence_nr) {
            crate::runtime_metrics::record_snapshot_write_stale_skipped();
            return SnapshotEnqueueOutcome::StaleSkipped;
        }

        if existing.is_none() && pending.writes.len() >= self.capacity {
            crate::runtime_metrics::record_snapshot_write_dropped();
            return SnapshotEnqueueOutcome::Full;
        }

        let outcome = if existing.is_some() {
            SnapshotEnqueueOutcome::Coalesced
        } else {
            SnapshotEnqueueOutcome::Enqueued
        };
        if outcome == SnapshotEnqueueOutcome::Coalesced {
            crate::runtime_metrics::record_snapshot_write_coalesced();
        }

        pending.writes.insert(
            persistence_id.clone(),
            QueuedSnapshotWrite {
                persistence_id,
                sequence_nr,
                snapshot,
                enqueued_at: Instant::now(), // determinism-ok: production snapshot queue metric only
            },
        );
        crate::runtime_metrics::record_snapshot_write_queue_depth(pending.writes.len() as u64);
        drop(pending);

        self.notify.notify_one();
        outcome
    }

    pub(crate) fn applied_sequence(&self, persistence_id: &str) -> Option<u64> {
        self.applied_sequences
            .lock()
            .expect("snapshot applied sequence mutex poisoned")
            .get(persistence_id)
            .copied()
    }

    pub(crate) fn pending_sequence(&self, persistence_id: &str) -> Option<u64> {
        self.pending
            .lock()
            .expect("snapshot queue mutex poisoned")
            .writes
            .get(persistence_id)
            .map(|write| write.sequence_nr)
    }

    fn spawn_worker(self: &Arc<Self>) {
        let queue = Arc::clone(self);
        tokio::spawn(async move {
            queue.run().await;
        });
    }

    async fn run(self: Arc<Self>) {
        loop {
            let writes = self.take_batch();
            if writes.is_empty() {
                self.notify.notified().await;
                continue;
            }

            for write in writes {
                self.apply(write).await;
            }
        }
    }

    fn take_batch(&self) -> Vec<QueuedSnapshotWrite> {
        let mut pending = self.pending.lock().expect("snapshot queue mutex poisoned");
        let mut writes = Vec::with_capacity(self.drain_batch.min(pending.writes.len()));
        for _ in 0..self.drain_batch {
            let Some((_, write)) = pending.writes.pop_first() else {
                break;
            };
            writes.push(write);
        }
        crate::runtime_metrics::record_snapshot_write_queue_depth(pending.writes.len() as u64);
        writes
    }

    async fn apply(&self, write: QueuedSnapshotWrite) {
        let span = tracing::info_span!(
            "dispatch.phase.snapshot.queued",
            otel.name = "dispatch.phase.snapshot.queued",
            persistence_id = %write.persistence_id,
            sequence_nr = write.sequence_nr,
        );

        async move {
            let started_at = Instant::now(); // determinism-ok: production snapshot latency metric only
            crate::runtime_metrics::record_snapshot_write_started();
            crate::runtime_metrics::record_snapshot_write_queue_wait(
                started_at.duration_since(write.enqueued_at),
            );
            let result = self
                .store
                .save_snapshot(&write.persistence_id, write.sequence_nr, &write.snapshot)
                .await;
            let result_label = if result.is_ok() { "ok" } else { "error" };
            crate::runtime_metrics::record_snapshot_write_duration(
                result_label,
                started_at.elapsed(),
            );
            crate::runtime_metrics::record_snapshot_write_end_to_end_duration(
                result_label,
                write.enqueued_at.elapsed(),
            );

            match result {
                Ok(()) => {
                    crate::runtime_metrics::record_snapshot_write_applied_sequence(
                        write.sequence_nr,
                    );
                    self.record_applied_sequence(&write.persistence_id, write.sequence_nr);
                }
                Err(e) => {
                    crate::runtime_metrics::record_snapshot_write_error();
                    tracing::error!(
                        error = %e,
                        persistence_id = %write.persistence_id,
                        sequence_nr = write.sequence_nr,
                        "failed to persist queued snapshot"
                    );
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    self.requeue_failed_write(write);
                }
            }
        }
        .instrument(span)
        .await;
    }

    fn record_applied_sequence(&self, persistence_id: &str, sequence_nr: u64) {
        let mut applied = self
            .applied_sequences
            .lock()
            .expect("snapshot applied sequence mutex poisoned");
        let entry = applied.entry(persistence_id.to_string()).or_insert(0);
        if sequence_nr > *entry {
            *entry = sequence_nr;
        }
    }

    fn requeue_failed_write(&self, write: QueuedSnapshotWrite) {
        let mut pending = self.pending.lock().expect("snapshot queue mutex poisoned");
        if pending
            .writes
            .get(&write.persistence_id)
            .is_some_and(|existing| existing.sequence_nr >= write.sequence_nr)
        {
            crate::runtime_metrics::record_snapshot_write_queue_depth(pending.writes.len() as u64);
            return;
        }
        pending.writes.insert(write.persistence_id.clone(), write);
        crate::runtime_metrics::record_snapshot_write_queue_depth(pending.writes.len() as u64);
        drop(pending);
        self.notify.notify_one();
    }
}

fn snapshot_queue_capacity() -> usize {
    std::env::var("TEMPER_SNAPSHOT_QUEUE_CAPACITY") // determinism-ok: production side-effect queue sizing
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_SNAPSHOT_QUEUE_CAPACITY)
}

fn snapshot_drain_batch() -> usize {
    std::env::var("TEMPER_SNAPSHOT_DRAIN_BATCH") // determinism-ok: production side-effect queue sizing
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_SNAPSHOT_DRAIN_BATCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use temper_runtime::persistence::{
        EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope,
        PersistenceError,
    };

    #[derive(Default)]
    struct RecordingEventStore {
        saves: AtomicUsize,
    }

    impl EventStore for RecordingEventStore {
        async fn append(
            &self,
            _persistence_id: &str,
            expected_sequence: u64,
            events: &[PersistenceEnvelope],
        ) -> Result<u64, PersistenceError> {
            Ok(expected_sequence + events.len() as u64)
        }

        async fn append_batch(
            &self,
            appends: &[PersistenceAppend],
        ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
            Ok(appends
                .iter()
                .map(|append| PersistenceAppendResult {
                    persistence_id: append.persistence_id.clone(),
                    sequence_nr: append.expected_sequence + append.events.len() as u64,
                })
                .collect())
        }

        async fn read_events(
            &self,
            _persistence_id: &str,
            _from_sequence: u64,
        ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
            Ok(Vec::new())
        }

        async fn save_snapshot(
            &self,
            _persistence_id: &str,
            _sequence_nr: u64,
            _snapshot: &[u8],
        ) -> Result<(), PersistenceError> {
            self.saves.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        async fn load_snapshot(
            &self,
            _persistence_id: &str,
        ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
            Ok(None)
        }

        async fn list_entity_ids(
            &self,
            _tenant: &str,
        ) -> Result<Vec<(String, String)>, PersistenceError> {
            Ok(Vec::new())
        }

        async fn list_entity_ids_by_type(
            &self,
            _tenant: &str,
            _entity_type: &str,
        ) -> Result<Vec<String>, PersistenceError> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn enqueue_coalesces_newer_snapshot_for_same_stream() {
        let queue = SnapshotWriteQueue::new_for_test(
            BoxedEventStore::new(RecordingEventStore::default()),
            10,
            10,
        );

        assert_eq!(
            queue.enqueue("tenant:Session:s-1".to_string(), 1, vec![1]),
            SnapshotEnqueueOutcome::Enqueued
        );
        assert_eq!(
            queue.enqueue("tenant:Session:s-1".to_string(), 2, vec![2]),
            SnapshotEnqueueOutcome::Coalesced
        );

        let writes = queue.take_batch();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].sequence_nr, 2);
        assert_eq!(writes[0].snapshot, vec![2]);
    }

    #[test]
    fn enqueue_skips_stale_snapshot_before_store_access() {
        let queue = SnapshotWriteQueue::new_for_test(
            BoxedEventStore::new(RecordingEventStore::default()),
            10,
            10,
        );

        assert_eq!(
            queue.enqueue("tenant:Session:s-1".to_string(), 3, vec![3]),
            SnapshotEnqueueOutcome::Enqueued
        );
        assert_eq!(
            queue.enqueue("tenant:Session:s-1".to_string(), 2, vec![2]),
            SnapshotEnqueueOutcome::StaleSkipped
        );

        let writes = queue.take_batch();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].sequence_nr, 3);
    }

    #[test]
    fn enqueue_rejects_new_stream_when_capacity_is_exhausted() {
        let queue = SnapshotWriteQueue::new_for_test(
            BoxedEventStore::new(RecordingEventStore::default()),
            1,
            10,
        );

        assert_eq!(
            queue.enqueue("tenant:Session:s-1".to_string(), 1, vec![1]),
            SnapshotEnqueueOutcome::Enqueued
        );
        assert_eq!(
            queue.enqueue("tenant:Session:s-2".to_string(), 1, vec![1]),
            SnapshotEnqueueOutcome::Full
        );
    }
}
