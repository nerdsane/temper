//! Bounded, coalescing snapshot writer for entity actors.
//!
//! The event journal append remains synchronous. Snapshot rows are derived
//! recovery accelerators, so this queue moves their writes off the actor hot
//! path while preserving the latest accepted sequence per persistence stream.

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use temper_runtime::persistence::{PersistenceError, SnapshotSourceFence};
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
    source: SnapshotSourceFence,
    key_contract: Option<String>,
    enqueued_at: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SnapshotWriteBoundary {
    activation_epoch: u64,
    sequence_nr: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AppliedSnapshotWrite {
    boundary: SnapshotWriteBoundary,
    snapshot_sha256: [u8; 32],
}

fn activation_epoch(key_contract: Option<&str>) -> u64 {
    key_contract
        .and_then(|contract| temper_runtime::persistence::decode_activated_key_contract(contract).1)
        .unwrap_or(0)
}

fn boundary_precedes(
    existing_contract: Option<&str>,
    existing_sequence: u64,
    incoming_contract: Option<&str>,
    incoming_sequence: u64,
) -> bool {
    let existing_epoch = activation_epoch(existing_contract);
    let incoming_epoch = activation_epoch(incoming_contract);
    existing_epoch < incoming_epoch
        || (existing_epoch == incoming_epoch && existing_sequence < incoming_sequence)
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
    applied_snapshots: Arc<Mutex<BTreeMap<String, AppliedSnapshotWrite>>>,
    notify: Arc<Notify>,
    capacity: usize,
    drain_batch: usize,
}

impl SnapshotWriteQueue {
    pub(crate) fn start(store: BoxedEventStore) -> Arc<Self> {
        let queue = Arc::new(Self {
            store,
            pending: Arc::new(Mutex::new(PendingSnapshotWrites::default())),
            applied_snapshots: Arc::new(Mutex::new(BTreeMap::new())),
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
            applied_snapshots: Arc::new(Mutex::new(BTreeMap::new())),
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
        source: SnapshotSourceFence,
        key_contract: Option<String>,
    ) -> SnapshotEnqueueOutcome {
        if matches!(
            &source,
            SnapshotSourceFence::Exact {
                sequence_nr: source_sequence,
                ..
            } if *source_sequence > sequence_nr
        ) {
            crate::runtime_metrics::record_snapshot_write_stale_skipped();
            return SnapshotEnqueueOutcome::StaleSkipped;
        }
        let mut pending = self.pending.lock().expect("snapshot queue mutex poisoned");
        let existing = pending.writes.get(&persistence_id);
        if existing.is_some_and(|existing| {
            !boundary_precedes(
                existing.key_contract.as_deref(),
                existing.sequence_nr,
                key_contract.as_deref(),
                sequence_nr,
            )
        }) {
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
                source,
                key_contract,
                enqueued_at: Instant::now(),
            },
        );
        crate::runtime_metrics::record_snapshot_write_queue_depth(pending.writes.len() as u64);
        drop(pending);

        self.notify.notify_one();
        outcome
    }

    #[cfg(test)]
    pub(crate) fn applied_sequence(&self, persistence_id: &str) -> Option<u64> {
        self.applied_snapshots
            .lock()
            .expect("snapshot applied sequence mutex poisoned")
            .get(persistence_id)
            .map(|applied| applied.boundary.sequence_nr)
    }

    pub(crate) fn applied_sequence_for_contract(
        &self,
        persistence_id: &str,
        key_contract: Option<&str>,
    ) -> Option<u64> {
        let incoming_epoch = activation_epoch(key_contract);
        self.applied_snapshots
            .lock()
            .expect("snapshot applied sequence mutex poisoned")
            .get(persistence_id)
            .filter(|applied| applied.boundary.activation_epoch >= incoming_epoch)
            .map(|applied| applied.boundary.sequence_nr)
    }

    pub(crate) async fn applied_source_for_contract(
        &self,
        persistence_id: &str,
        key_contract: Option<&str>,
    ) -> Option<SnapshotSourceFence> {
        let incoming_epoch = activation_epoch(key_contract);
        let applied = self
            .applied_snapshots
            .lock()
            .expect("snapshot applied source mutex poisoned")
            .get(persistence_id)
            .filter(|applied| applied.boundary.activation_epoch == incoming_epoch)
            .cloned()?;
        let (sequence_nr, state) = self.store.load_snapshot(persistence_id).await.ok()??;
        let digest: [u8; 32] = Sha256::digest(&state).into();
        if sequence_nr != applied.boundary.sequence_nr || digest != applied.snapshot_sha256 {
            return None;
        }
        Some(SnapshotSourceFence::Exact { sequence_nr, state })
    }

    #[cfg(test)]
    pub(crate) fn pending_sequence(&self, persistence_id: &str) -> Option<u64> {
        self.pending
            .lock()
            .expect("snapshot queue mutex poisoned")
            .writes
            .get(persistence_id)
            .map(|write| write.sequence_nr)
    }

    pub(crate) fn pending_sequence_for_contract(
        &self,
        persistence_id: &str,
        key_contract: Option<&str>,
    ) -> Option<u64> {
        let incoming_epoch = activation_epoch(key_contract);
        self.pending
            .lock()
            .expect("snapshot queue mutex poisoned")
            .writes
            .get(persistence_id)
            .filter(|write| activation_epoch(write.key_contract.as_deref()) >= incoming_epoch)
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

    async fn apply(&self, mut write: QueuedSnapshotWrite) {
        let span = tracing::info_span!(
            "dispatch.phase.snapshot.queued",
            otel.name = "dispatch.phase.snapshot.queued",
            persistence_id = %write.persistence_id,
            sequence_nr = write.sequence_nr,
        );

        async move {
            let started_at = Instant::now();
            crate::runtime_metrics::record_snapshot_write_started();
            crate::runtime_metrics::record_snapshot_write_queue_wait(
                started_at.duration_since(write.enqueued_at),
            );
            if let Some(applied_source) = self
                .applied_source_for_contract(&write.persistence_id, write.key_contract.as_deref())
                .await
            {
                let SnapshotSourceFence::Exact {
                    sequence_nr: applied_sequence,
                    ..
                } = &applied_source
                else {
                    unreachable!("an applied snapshot source is always exact");
                };
                if *applied_sequence > write.sequence_nr {
                    crate::runtime_metrics::record_snapshot_write_stale_skipped();
                    return;
                }
                let supplied_sequence = match &write.source {
                    SnapshotSourceFence::Exact { sequence_nr, .. } => Some(*sequence_nr),
                    SnapshotSourceFence::Absent | SnapshotSourceFence::Unchecked => None,
                };
                if supplied_sequence.is_none_or(|sequence_nr| *applied_sequence >= sequence_nr) {
                    write.source = applied_source;
                }
            }
            let result = self
                .store
                .save_snapshot_if_source(
                    &write.persistence_id,
                    write.sequence_nr,
                    &write.snapshot,
                    &write.source,
                    write.key_contract.as_deref(),
                )
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
                    self.record_applied_snapshot(
                        &write.persistence_id,
                        write.sequence_nr,
                        &write.snapshot,
                        write.key_contract.as_deref(),
                    );
                }
                Err(PersistenceError::SnapshotGenerationChanged) => {
                    crate::runtime_metrics::record_snapshot_write_stale_skipped();
                    tracing::warn!(
                        persistence_id = %write.persistence_id,
                        sequence_nr = write.sequence_nr,
                        "discarding queued snapshot because its source generation changed"
                    );
                }
                Err(
                    PersistenceError::KeyContractNotActive { .. }
                    | PersistenceError::KeyContractActivationStale { .. },
                ) => {
                    crate::runtime_metrics::record_snapshot_write_stale_skipped();
                    tracing::warn!(
                        persistence_id = %write.persistence_id,
                        sequence_nr = write.sequence_nr,
                        "discarding queued snapshot because its key contract is stale"
                    );
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

    fn record_applied_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
        key_contract: Option<&str>,
    ) {
        let mut applied = self
            .applied_snapshots
            .lock()
            .expect("snapshot applied sequence mutex poisoned");
        let incoming = AppliedSnapshotWrite {
            boundary: SnapshotWriteBoundary {
                activation_epoch: activation_epoch(key_contract),
                sequence_nr,
            },
            snapshot_sha256: Sha256::digest(snapshot).into(),
        };
        let entry = applied
            .entry(persistence_id.to_string())
            .or_insert_with(|| incoming.clone());
        if entry.boundary.activation_epoch < incoming.boundary.activation_epoch
            || (entry.boundary.activation_epoch == incoming.boundary.activation_epoch
                && entry.boundary.sequence_nr < incoming.boundary.sequence_nr)
        {
            *entry = incoming;
        }
    }

    #[cfg(test)]
    fn record_applied_sequence(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        key_contract: Option<&str>,
    ) {
        self.record_applied_snapshot(persistence_id, sequence_nr, &[], key_contract);
    }

    fn requeue_failed_write(&self, write: QueuedSnapshotWrite) {
        let mut pending = self.pending.lock().expect("snapshot queue mutex poisoned");
        if pending
            .writes
            .get(&write.persistence_id)
            .is_some_and(|existing| {
                !boundary_precedes(
                    existing.key_contract.as_deref(),
                    existing.sequence_nr,
                    write.key_contract.as_deref(),
                    write.sequence_nr,
                )
            })
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
#[path = "snapshot_queue_test.rs"]
mod tests;
