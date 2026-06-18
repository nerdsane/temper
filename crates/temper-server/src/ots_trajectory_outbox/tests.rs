use super::*;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use tokio::sync::Notify;

#[derive(Default)]
struct FakeOtsStore {
    attempts: AtomicU64,
    fail_attempts: AtomicU64,
    enqueue_attempts: AtomicU64,
    mark_persisted_attempts: AtomicU64,
    mark_failed_attempts: AtomicU64,
    status: Mutex<BTreeMap<String, String>>,
    persist_started: Notify,
    allow_persist: Notify,
    block_first_persist: AtomicBool,
}

#[async_trait::async_trait]
impl OtsStore for FakeOtsStore {
    async fn persist_ots_trajectory(
        &self,
        _params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        self.persist_started.notify_waiters();
        if self.block_first_persist.swap(false, Ordering::Relaxed) {
            self.allow_persist.notified().await;
        }
        if self.fail_attempts.load(Ordering::Relaxed) > 0 {
            self.fail_attempts.fetch_sub(1, Ordering::Relaxed);
            return Err(PersistenceError::Storage(
                "temporary OTS failure".to_string(),
            ));
        }
        Ok(())
    }

    async fn enqueue_ots_trajectory(
        &self,
        params: &OtsTrajectoryParams<'_>,
    ) -> Result<(), PersistenceError> {
        self.enqueue_attempts.fetch_add(1, Ordering::Relaxed);
        self.status
            .lock()
            .expect("fake status mutex poisoned")
            .insert(params.trajectory_id.to_string(), "queued".to_string());
        Ok(())
    }

    async fn mark_ots_trajectory_persisted(
        &self,
        trajectory_id: &str,
    ) -> Result<(), PersistenceError> {
        self.attempts.fetch_add(1, Ordering::Relaxed);
        self.mark_persisted_attempts.fetch_add(1, Ordering::Relaxed);
        self.persist_started.notify_waiters();
        if self.block_first_persist.swap(false, Ordering::Relaxed) {
            self.allow_persist.notified().await;
        }
        if self.fail_attempts.load(Ordering::Relaxed) > 0 {
            self.fail_attempts.fetch_sub(1, Ordering::Relaxed);
            return Err(PersistenceError::Storage(
                "temporary OTS failure".to_string(),
            ));
        }
        self.status
            .lock()
            .expect("fake status mutex poisoned")
            .insert(trajectory_id.to_string(), "persisted".to_string());
        Ok(())
    }

    async fn mark_ots_trajectory_failed(
        &self,
        trajectory_id: &str,
        _error: &str,
    ) -> Result<(), PersistenceError> {
        self.mark_failed_attempts.fetch_add(1, Ordering::Relaxed);
        self.status
            .lock()
            .expect("fake status mutex poisoned")
            .insert(trajectory_id.to_string(), "failed".to_string());
        Ok(())
    }

    async fn list_queued_ots_trajectories(
        &self,
        _limit: i64,
    ) -> Result<Vec<temper_store_turso::OtsQueuedTrajectoryRow>, PersistenceError> {
        Ok(Vec::new())
    }

    async fn list_ots_trajectories(
        &self,
        _tenant: &str,
        _agent_id: Option<&str>,
        _outcome: Option<&str>,
        _limit: i64,
    ) -> Result<Vec<temper_store_turso::OtsTrajectoryRow>, PersistenceError> {
        Ok(Vec::new())
    }

    async fn get_ots_trajectory(
        &self,
        _trajectory_id: &str,
    ) -> Result<Option<String>, PersistenceError> {
        Ok(None)
    }
}

fn status(store: &FakeOtsStore, id: &str) -> Option<String> {
    store
        .status
        .lock()
        .expect("fake status mutex poisoned")
        .get(id)
        .cloned()
}

fn item(id: &str) -> OtsTrajectoryWrite {
    OtsTrajectoryWrite {
        trajectory_id: id.to_string(),
        tenant: "tenant".to_string(),
        agent_id: "agent".to_string(),
        session_id: "session".to_string(),
        outcome: "success".to_string(),
        turn_count: 1,
        data: serde_json::json!({
            "metadata": {
                "trajectory_id": id,
                "agent_id": "agent",
                "outcome": "success"
            },
            "turns": []
        })
        .to_string(),
    }
}

#[tokio::test]
async fn enqueue_does_not_wait_for_slow_persistence() {
    let store = Arc::new(FakeOtsStore {
        block_first_persist: AtomicBool::new(true),
        ..FakeOtsStore::default()
    });
    let outbox = OtsTrajectoryOutbox::start_for_tests(2, 1, 2, Duration::from_millis(1));

    outbox
        .try_enqueue_for_tests(store.clone(), item("traj-slow"))
        .expect("enqueue should accept slow persistence");

    tokio::time::timeout(Duration::from_millis(100), store.persist_started.notified())
        .await
        .expect("background worker should start persistence");
    assert_eq!(outbox.depth(), 1);
    assert_eq!(store.attempts.load(Ordering::Relaxed), 1);

    store.allow_persist.notify_waiters();
    tokio::time::timeout(Duration::from_millis(100), async {
        while outbox.depth() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("outbox should drain after the store unblocks");
}

#[tokio::test]
async fn durable_admission_records_queued_before_enqueue_ack() {
    let store = Arc::new(FakeOtsStore {
        block_first_persist: AtomicBool::new(true),
        ..FakeOtsStore::default()
    });
    let outbox = OtsTrajectoryOutbox::start_for_tests(2, 1, 2, Duration::from_millis(1));

    outbox
        .try_enqueue_durable_for_tests(store.clone(), item("traj-durable"))
        .await
        .expect("durable enqueue should accept artifact");

    assert_eq!(store.enqueue_attempts.load(Ordering::Relaxed), 1);
    assert_eq!(status(&store, "traj-durable").as_deref(), Some("queued"));
    store.allow_persist.notify_waiters();
}

#[tokio::test]
async fn failed_persistence_is_retried() {
    let store = Arc::new(FakeOtsStore {
        fail_attempts: AtomicU64::new(1),
        ..FakeOtsStore::default()
    });
    let outbox = OtsTrajectoryOutbox::start_for_tests(2, 1, 3, Duration::from_millis(1));

    outbox
        .try_enqueue_for_tests(store.clone(), item("traj-retry"))
        .expect("enqueue should accept retryable persistence");

    tokio::time::timeout(Duration::from_millis(200), async {
        while outbox.depth() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("retry should eventually persist");
    assert_eq!(store.attempts.load(Ordering::Relaxed), 2);
    assert_eq!(outbox.failed_total(), 0);
    assert_eq!(status(&store, "traj-retry").as_deref(), Some("persisted"));
}

#[tokio::test]
async fn exhausted_retries_are_visible() {
    let store = Arc::new(FakeOtsStore {
        fail_attempts: AtomicU64::new(3),
        ..FakeOtsStore::default()
    });
    let outbox = OtsTrajectoryOutbox::start_for_tests(2, 1, 2, Duration::from_millis(1));

    outbox
        .try_enqueue_for_tests(store.clone(), item("traj-fail"))
        .expect("enqueue should accept artifact before retries exhaust");

    tokio::time::timeout(Duration::from_millis(200), async {
        while outbox.depth() != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("terminal failure should release queue depth");
    assert_eq!(store.attempts.load(Ordering::Relaxed), 2);
    assert_eq!(outbox.failed_total(), 1);
    assert_eq!(store.mark_failed_attempts.load(Ordering::Relaxed), 1);
    assert_eq!(status(&store, "traj-fail").as_deref(), Some("failed"));
}

#[tokio::test]
async fn full_outbox_rejects_for_caller_retry() {
    let store = Arc::new(FakeOtsStore {
        block_first_persist: AtomicBool::new(true),
        ..FakeOtsStore::default()
    });
    let outbox = OtsTrajectoryOutbox::start_for_tests(1, 1, 2, Duration::from_millis(1));

    outbox
        .try_enqueue_for_tests(store, item("traj-one"))
        .expect("first enqueue should fit");
    assert_eq!(
        outbox.try_enqueue_for_tests(Arc::new(FakeOtsStore::default()), item("traj-two")),
        Err(OtsTrajectoryEnqueueError::Full)
    );
    assert_eq!(outbox.rejected_total(), 1);
    assert_eq!(outbox.depth(), 1);
}
