use super::*;
use std::sync::atomic::AtomicU8;
use std::sync::atomic::{AtomicUsize, Ordering};
use temper_runtime::persistence::{
    EventStore, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
    SnapshotSourceFence, encode_activated_key_contract,
};

#[derive(Default)]
struct RecordingEventStore {
    saves: AtomicUsize,
}

struct ErroringSnapshotStore {
    failure: AtomicU8,
}

#[cfg(feature = "sim")]
struct BlockingSnapshotStore {
    inner: temper_store_sim::SimEventStore,
    saves: AtomicUsize,
    first_started: tokio::sync::Notify,
    resume_first: tokio::sync::Notify,
}

#[cfg(feature = "sim")]
impl BlockingSnapshotStore {
    fn new() -> Self {
        Self {
            inner: temper_store_sim::SimEventStore::no_faults(45),
            saves: AtomicUsize::new(0),
            first_started: tokio::sync::Notify::new(),
            resume_first: tokio::sync::Notify::new(),
        }
    }
}

impl ErroringSnapshotStore {
    fn new(failure: u8) -> Self {
        Self {
            failure: AtomicU8::new(failure),
        }
    }
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
                batch_already_applied: false,
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

    async fn read_events_page(
        &self,
        _persistence_id: &str,
        _from_sequence: u64,
        _through_sequence: u64,
        _limit: usize,
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

impl EventStore for ErroringSnapshotStore {
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
                batch_already_applied: false,
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

    async fn read_events_page(
        &self,
        _persistence_id: &str,
        _from_sequence: u64,
        _through_sequence: u64,
        _limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        Ok(Vec::new())
    }

    async fn save_snapshot(
        &self,
        _persistence_id: &str,
        _sequence_nr: u64,
        _snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        Ok(())
    }

    async fn save_snapshot_if_source(
        &self,
        _persistence_id: &str,
        _sequence_nr: u64,
        _snapshot: &[u8],
        _source: &SnapshotSourceFence,
        _key_contract: Option<&str>,
    ) -> Result<(), PersistenceError> {
        match self.failure.load(Ordering::SeqCst) {
            1 => Err(PersistenceError::SnapshotGenerationChanged),
            2 => Err(PersistenceError::KeyContractNotActive {
                activated_signature: "active".to_string(),
                attempted_signature: "stale".to_string(),
            }),
            3 => Err(PersistenceError::KeyContractActivationNotReady {
                activated_epoch: 2,
                activated_signature: "active".to_string(),
            }),
            _ => Ok(()),
        }
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

#[cfg(feature = "sim")]
impl EventStore for BlockingSnapshotStore {
    async fn append(
        &self,
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.inner
            .append(persistence_id, expected_sequence, events)
            .await
    }

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        self.inner.append_batch(appends).await
    }

    async fn read_events(
        &self,
        persistence_id: &str,
        from_sequence: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.inner.read_events(persistence_id, from_sequence).await
    }

    async fn read_events_page(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        through_sequence: u64,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.inner
            .read_events_page(persistence_id, from_sequence, through_sequence, limit)
            .await
    }

    async fn save_snapshot(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
    ) -> Result<(), PersistenceError> {
        self.inner
            .save_snapshot(persistence_id, sequence_nr, snapshot)
            .await
    }

    async fn save_snapshot_if_source(
        &self,
        persistence_id: &str,
        sequence_nr: u64,
        snapshot: &[u8],
        source: &SnapshotSourceFence,
        key_contract: Option<&str>,
    ) -> Result<(), PersistenceError> {
        if self.saves.fetch_add(1, Ordering::SeqCst) == 0 {
            self.first_started.notify_one();
            self.resume_first.notified().await;
        }
        self.inner
            .save_snapshot_if_source(persistence_id, sequence_nr, snapshot, source, key_contract)
            .await
    }

    async fn load_snapshot(
        &self,
        persistence_id: &str,
    ) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        self.inner.load_snapshot(persistence_id).await
    }

    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.inner.list_entity_ids(tenant).await
    }

    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        entity_type: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.inner
            .list_entity_ids_by_type(tenant, entity_type)
            .await
    }
}

fn activated_contract(epoch: u64) -> String {
    encode_activated_key_contract("v3|path:WorkspaceId,Path", epoch)
}

#[test]
fn enqueue_coalesces_newer_snapshot_for_same_stream() {
    let queue = SnapshotWriteQueue::new_for_test(
        BoxedEventStore::new(RecordingEventStore::default()),
        10,
        10,
    );

    assert_eq!(
        queue.enqueue(
            "tenant:Session:s-1".to_string(),
            1,
            vec![1],
            SnapshotSourceFence::Unchecked,
            None,
        ),
        SnapshotEnqueueOutcome::Enqueued
    );
    assert_eq!(
        queue.enqueue(
            "tenant:Session:s-1".to_string(),
            2,
            vec![2],
            SnapshotSourceFence::Unchecked,
            None,
        ),
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
        queue.enqueue(
            "tenant:Session:s-1".to_string(),
            3,
            vec![3],
            SnapshotSourceFence::Unchecked,
            None,
        ),
        SnapshotEnqueueOutcome::Enqueued
    );
    assert_eq!(
        queue.enqueue(
            "tenant:Session:s-1".to_string(),
            2,
            vec![2],
            SnapshotSourceFence::Unchecked,
            None,
        ),
        SnapshotEnqueueOutcome::StaleSkipped
    );

    let writes = queue.take_batch();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].sequence_nr, 3);
}

#[test]
fn enqueue_skips_snapshot_below_exact_source_sequence() {
    let queue = SnapshotWriteQueue::new_for_test(
        BoxedEventStore::new(RecordingEventStore::default()),
        10,
        10,
    );

    assert_eq!(
        queue.enqueue(
            "tenant:Session:snapshot-ahead".to_string(),
            2,
            vec![2],
            SnapshotSourceFence::Exact {
                sequence_nr: 5,
                state: vec![5],
            },
            None,
        ),
        SnapshotEnqueueOutcome::StaleSkipped
    );
    assert_eq!(
        queue.pending_sequence("tenant:Session:snapshot-ahead"),
        None,
        "the queue must not report an older monotonic no-op as pending"
    );
    assert!(queue.take_batch().is_empty());
}

#[test]
fn enqueue_rejects_new_stream_when_capacity_is_exhausted() {
    let queue = SnapshotWriteQueue::new_for_test(
        BoxedEventStore::new(RecordingEventStore::default()),
        1,
        10,
    );

    assert_eq!(
        queue.enqueue(
            "tenant:Session:s-1".to_string(),
            1,
            vec![1],
            SnapshotSourceFence::Unchecked,
            None,
        ),
        SnapshotEnqueueOutcome::Enqueued
    );
    assert_eq!(
        queue.enqueue(
            "tenant:Session:s-2".to_string(),
            1,
            vec![1],
            SnapshotSourceFence::Unchecked,
            None,
        ),
        SnapshotEnqueueOutcome::Full
    );
}

#[test]
fn higher_epoch_replaces_equal_sequence_pending_snapshot() {
    let queue = SnapshotWriteQueue::new_for_test(
        BoxedEventStore::new(RecordingEventStore::default()),
        10,
        10,
    );
    let persistence_id = "tenant:Doc:epoch-replacement";
    assert_eq!(
        queue.enqueue(
            persistence_id.to_string(),
            7,
            vec![1],
            SnapshotSourceFence::Absent,
            Some(activated_contract(1)),
        ),
        SnapshotEnqueueOutcome::Enqueued
    );
    assert_eq!(
        queue.enqueue(
            persistence_id.to_string(),
            7,
            vec![2],
            SnapshotSourceFence::Absent,
            Some(activated_contract(2)),
        ),
        SnapshotEnqueueOutcome::Coalesced
    );
    let writes = queue.take_batch();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].snapshot, vec![2]);
    assert_eq!(writes[0].key_contract, Some(activated_contract(2)));
}

#[test]
fn lower_epoch_cannot_replace_or_suppress_higher_epoch_snapshot() {
    let queue = SnapshotWriteQueue::new_for_test(
        BoxedEventStore::new(RecordingEventStore::default()),
        10,
        10,
    );
    let persistence_id = "tenant:Doc:epoch-order";
    assert_eq!(
        queue.enqueue(
            persistence_id.to_string(),
            4,
            vec![2],
            SnapshotSourceFence::Absent,
            Some(activated_contract(2)),
        ),
        SnapshotEnqueueOutcome::Enqueued
    );
    assert_eq!(
        queue.enqueue(
            persistence_id.to_string(),
            99,
            vec![1],
            SnapshotSourceFence::Absent,
            Some(activated_contract(1)),
        ),
        SnapshotEnqueueOutcome::StaleSkipped
    );
    let writes = queue.take_batch();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].key_contract, Some(activated_contract(2)));
}

#[test]
fn old_epoch_pending_and_applied_boundaries_do_not_throttle_new_epoch() {
    let queue = SnapshotWriteQueue::new_for_test(
        BoxedEventStore::new(RecordingEventStore::default()),
        10,
        10,
    );
    let persistence_id = "tenant:Doc:epoch-boundary";
    let old_contract = activated_contract(1);
    let new_contract = activated_contract(2);
    queue.enqueue(
        persistence_id.to_string(),
        100,
        vec![1],
        SnapshotSourceFence::Absent,
        Some(old_contract.clone()),
    );
    queue.record_applied_sequence(persistence_id, 100, Some(&old_contract));

    assert_eq!(
        queue.pending_sequence_for_contract(persistence_id, Some(&new_contract)),
        None
    );
    assert_eq!(
        queue.applied_sequence_for_contract(persistence_id, Some(&new_contract)),
        None
    );
    assert_eq!(
        queue.pending_sequence_for_contract(persistence_id, Some(&old_contract)),
        Some(100)
    );
    assert_eq!(
        queue.applied_sequence_for_contract(persistence_id, Some(&old_contract)),
        Some(100)
    );
}

#[cfg(feature = "sim")]
#[tokio::test]
async fn in_flight_snapshot_rebases_the_already_pending_next_write() {
    let store = Arc::new(BlockingSnapshotStore::new());
    let queue = Arc::new(SnapshotWriteQueue::new_for_test(
        BoxedEventStore::from_arc(store.clone()),
        10,
        10,
    ));
    let persistence_id = "tenant:Doc:in-flight-source-chain";
    assert_eq!(
        queue.enqueue(
            persistence_id.to_string(),
            1,
            vec![1],
            SnapshotSourceFence::Absent,
            None,
        ),
        SnapshotEnqueueOutcome::Enqueued
    );
    let first = queue.take_batch().pop().expect("first queued write");
    let apply_queue = queue.clone();
    let first_apply = tokio::spawn(async move { apply_queue.apply(first).await });
    store.first_started.notified().await;

    assert_eq!(
        queue.enqueue(
            persistence_id.to_string(),
            2,
            vec![2],
            SnapshotSourceFence::Absent,
            None,
        ),
        SnapshotEnqueueOutcome::Enqueued,
        "the second write must be pending before the first one commits"
    );
    store.resume_first.notify_one();
    first_apply.await.expect("first apply task");

    let second = queue.take_batch().pop().expect("second queued write");
    queue.apply(second).await;
    assert_eq!(queue.applied_sequence(persistence_id), Some(2));
    assert_eq!(
        store
            .load_snapshot(persistence_id)
            .await
            .expect("load final snapshot"),
        Some((2, vec![2]))
    );
}

#[tokio::test]
async fn terminal_contract_errors_drop_but_not_ready_requeues() {
    for terminal_failure in [1, 2] {
        let queue = SnapshotWriteQueue::new_for_test(
            BoxedEventStore::new(ErroringSnapshotStore::new(terminal_failure)),
            10,
            10,
        );
        queue.enqueue(
            format!("tenant:Doc:terminal-{terminal_failure}"),
            1,
            vec![1],
            SnapshotSourceFence::Absent,
            Some(activated_contract(1)),
        );
        let write = queue.take_batch().pop().expect("queued terminal write");
        queue.apply(write).await;
        assert!(queue.take_batch().is_empty());
    }

    let queue = SnapshotWriteQueue::new_for_test(
        BoxedEventStore::new(ErroringSnapshotStore::new(3)),
        10,
        10,
    );
    queue.enqueue(
        "tenant:Doc:not-ready".to_string(),
        1,
        vec![1],
        SnapshotSourceFence::Absent,
        Some(activated_contract(2)),
    );
    let write = queue.take_batch().pop().expect("queued not-ready write");
    queue.apply(write).await;
    assert_eq!(queue.pending_sequence("tenant:Doc:not-ready"), Some(1));
}
