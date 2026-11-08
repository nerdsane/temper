//! A person's move that lands while a stop is being decided wins: the stop
//! read the journal before the move, so it must not say the record needs a
//! person after the person moved it.

use super::*;
use temper_runtime::persistence::{
    CommitRows, EventStore, JournalHead, PersistenceAppend, PersistenceAppendResult,
    PersistenceEnvelope,
};
use temper_server::storage::BoxedEventStore;
use tokio::sync::Notify;

/// The journal, with one read of a record's newest events held after it is
/// read until the test lets it go: the stop decides on what it read, while the
/// record moves on.
#[derive(Clone)]
struct HeldRead {
    journal: SimEventStore,
    /// The record whose next newest-events read is held.
    hold: Arc<Mutex<Option<String>>>,
    reached: Arc<Notify>,
    go: Arc<Notify>,
}

impl EventStore for HeldRead {
    async fn append(
        &self,
        id: &str,
        seq: u64,
        events: &[PersistenceEnvelope],
    ) -> Result<u64, PersistenceError> {
        self.journal.append(id, seq, events).await
    }
    async fn append_with_rows(
        &self,
        id: &str,
        seq: u64,
        events: &[PersistenceEnvelope],
        rows: CommitRows<'_>,
    ) -> Result<u64, PersistenceError> {
        self.journal.append_with_rows(id, seq, events, rows).await
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
        let page = self
            .journal
            .read_events_page(id, before_sequence, limit)
            .await;
        let held = before_sequence.is_none() && {
            let mut hold = self.hold.lock().unwrap();
            if hold.as_deref() == Some(id) {
                *hold = None;
                true
            } else {
                false
            }
        };
        if held {
            self.reached.notify_one();
            self.go.notified().await;
        }
        page
    }
    async fn first_event_at(
        &self,
        id: &str,
    ) -> Result<Option<chrono::DateTime<chrono::Utc>>, PersistenceError> {
        self.journal.first_event_at(id).await
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
    async fn latest_sequence_nr(&self, id: &str) -> Result<u64, PersistenceError> {
        self.journal.latest_sequence_nr(id).await
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

/// A system-made Check fires `Finish`, which binds its caller, and is refused.
/// While the stop that follows holds what it read of the journal, a person
/// moves the record. The person's move wins: the timer runs as them.
#[test]
fn a_persons_move_during_a_stop_runs_the_timer_as_them() {
    Sim::each_seed(|sim| {
        let journal = HeldRead {
            journal: sim.store.clone(),
            hold: Arc::new(Mutex::new(None)),
            reached: Arc::new(Notify::new()),
            go: Arc::new(Notify::new()),
        };
        let begin_at = sim.draw(500);
        let server = sim.start_over(1, Some(BoxedEventStore::new(journal.clone())));
        let state = &server.state;
        server.run(async {
            wait(begin_at).await;
            act(state, "Check", "c-race", "Begin", &AgentContext::system()).await;
            // Begin's own follow has read what it reads; the next read of this
            // record's newest events is the stop's, after the timer's fire is
            // refused.
            wait(Duration::from_millis(300)).await;
            *journal.hold.lock().unwrap() = Some("default:Check:c-race".to_string());
            tokio::time::timeout(WITHIN, journal.reached.notified())
                .await
                .expect("the refused fire's stop reads the journal");

            act(state, "Check", "c-race", "Nudge", &person("gil")).await;
            journal.go.notify_one();

            let record = until(state, "Checks", "c-race", "finished", |record| {
                record["status"] == "Done"
            })
            .await;
            assert_eq!(
                record["fields"]["fired_by"], "gil",
                "seed {}: {record}",
                sim.seed
            );
            assert_eq!(
                record["timer_needs_person"],
                Value::Null,
                "seed {}: {record}",
                sim.seed
            );
        });
    });
}
