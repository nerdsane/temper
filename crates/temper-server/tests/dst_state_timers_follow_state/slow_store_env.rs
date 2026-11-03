//! Simulated slow/failing disk. Only I/O is replaced.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use temper_runtime::persistence::{
    CommitRows, EventStore, JournalHead, PersistenceAppend, PersistenceAppendResult,
    PersistenceEnvelope, PersistenceError,
};
use temper_runtime::scheduler::DeterministicRng;
use temper_store_sim::SimEventStore;

#[derive(Default)]
pub(super) struct Activity {
    pub active: usize,
    pub peak: usize,
    pub failed: usize,
    pub slow: usize,
}

// Per record: expected journal sequence, append delay in seconds, failed.
type AppendAttempts = BTreeMap<String, Vec<(u64, u64, bool)>>;

#[derive(Clone)]
pub(super) struct SlowStore {
    pub journal: SimEventStore,
    pub activity: Arc<Mutex<Activity>>,
    rng: Arc<Mutex<DeterministicRng>>,
    pub faults: Arc<Mutex<bool>>,
    pub attempts: Arc<Mutex<AppendAttempts>>,
}

impl SlowStore {
    pub fn new(seed: u64) -> Self {
        Self {
            journal: SimEventStore::no_faults(seed),
            activity: Arc::new(Mutex::new(Activity::default())),
            rng: Arc::new(Mutex::new(DeterministicRng::new(seed ^ 0x510f))),
            faults: Arc::new(Mutex::new(false)),
            attempts: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }
}

impl EventStore for SlowStore {
    async fn append(
        &self,
        id: &str,
        seq: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.append_with_rows(id, seq, events, CommitRows::default())
            .await
    }

    async fn append_with_rows(
        &self,
        id: &str,
        seq: u64,
        events: &[PersistenceEnvelope],
        rows: CommitRows<'_>,
    ) -> Result<u64, PersistenceError> {
        if !*self.faults.lock().unwrap() {
            return self.journal.append_with_rows(id, seq, events, rows).await;
        }
        let first = self.activity.lock().unwrap().slow == 0;
        let (delay, fails) = {
            let mut rng = self.rng.lock().unwrap();
            // The first failure isolates rollback before a request deadline;
            // later successes and failures outlast the full 5s x 3 budget.
            if first {
                (Duration::from_secs(1), true)
            } else {
                (
                    Duration::from_secs(20 + rng.next_bound(21) as u64),
                    rng.chance(0.3),
                )
            }
        };
        {
            let mut a = self.activity.lock().unwrap();
            a.active += 1;
            a.peak = a.peak.max(a.active);
            a.slow += 1;
        }
        tokio::time::sleep(delay).await;
        let result = if fails {
            self.activity.lock().unwrap().failed += 1;
            Err(PersistenceError::Storage("turso.append timed out".into()))
        } else {
            self.journal.append_with_rows(id, seq, events, rows).await
        };
        self.attempts
            .lock()
            .unwrap()
            .entry(id.into())
            .or_default()
            .push((seq, delay.as_secs(), fails));
        self.activity.lock().unwrap().active -= 1;
        result
    }

    async fn append_batch(
        &self,
        appends: &[PersistenceAppend],
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        self.journal.append_batch(appends).await
    }
    async fn read_events(
        &self,
        id: &str,
        seq: u64,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.journal.read_events(id, seq).await
    }
    async fn read_events_page(
        &self,
        id: &str,
        before_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<PersistenceEnvelope>, PersistenceError> {
        self.journal
            .read_events_page(id, before_sequence, limit)
            .await
    }
    async fn save_snapshot(
        &self,
        id: &str,
        seq: u64,
        bytes: &[u8],
    ) -> Result<(), PersistenceError> {
        self.journal.save_snapshot(id, seq, bytes).await
    }
    async fn load_snapshot(&self, id: &str) -> Result<Option<(u64, Vec<u8>)>, PersistenceError> {
        self.journal.load_snapshot(id).await
    }
    async fn journal_heads(&self, tenant: &str) -> Result<Vec<JournalHead>, PersistenceError> {
        self.journal.journal_heads(tenant).await
    }
    async fn list_entity_ids(
        &self,
        tenant: &str,
    ) -> Result<Vec<(String, String)>, PersistenceError> {
        self.journal.list_entity_ids(tenant).await
    }
    async fn list_entity_ids_by_type(
        &self,
        tenant: &str,
        kind: &str,
    ) -> Result<Vec<String>, PersistenceError> {
        self.journal.list_entity_ids_by_type(tenant, kind).await
    }
}
