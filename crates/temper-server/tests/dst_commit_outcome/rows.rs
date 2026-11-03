//! The rows a record keeps beside its journal, and what they must say.
//!
//! The sim store keeps each record's query-plane row and state-timer row,
//! and counts the commits that write its journal or rows. The server reads
//! and writes timer rows through [`SimTimerRows`], a view of the same store,
//! so a row the server writes after the commit is a commit of its own.

use temper_runtime::persistence::{PersistenceError, StateTimerRecord};
use temper_server::storage::StateTimerStore;
use temper_store_sim::SimEventStore;

use super::journal_of;

/// The server's timer rows, kept in the sim store. Each write is a round trip.
pub struct SimTimerRows(pub SimEventStore);

async fn round_trip() {
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
}

#[async_trait::async_trait]
impl StateTimerStore for SimTimerRows {
    async fn upsert_state_timer(&self, timer: &StateTimerRecord) -> Result<(), PersistenceError> {
        round_trip().await;
        self.0.upsert_state_timer(timer);
        Ok(())
    }

    async fn delete_state_timer(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        arm_id: &str,
    ) -> Result<(), PersistenceError> {
        round_trip().await;
        self.0
            .delete_state_timer(tenant, entity_type, entity_id, arm_id);
        Ok(())
    }

    async fn list_state_timers(
        &self,
        tenant: &str,
    ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
        Ok(self.0.list_state_timers(tenant))
    }
}

/// A record's journal as its rows should show it.
pub struct Journal {
    /// The newest sequence.
    pub sequence: u64,
    /// The status the journal leaves the record in.
    pub status: String,
    /// The sequence of the event that entered `Busy` last, while it is there.
    pub busy_since: Option<u64>,
}

/// Whether `id`'s rows say what its journal says: its query-plane row is at
/// the journal's sequence and status, and it has a timer row exactly while
/// it is `Busy`, from the event that took it there. The broken invariant
/// and what was found.
pub fn rows_follow_journal(
    store: &SimEventStore,
    id: &str,
    journal: &Journal,
) -> Result<(), (&'static str, String)> {
    let pid = journal_of(id);
    match store.projection(&pid) {
        Some(row) if row.sequence_nr == journal.sequence && row.status == journal.status => {}
        row => {
            return Err((
                "a record's query-plane row is committed with its event",
                format!(
                    "{id}: journal at {} {}, row {row:?}",
                    journal.sequence, journal.status
                ),
            ));
        }
    }
    let timer = store.state_timer(&pid);
    let agrees = match (journal.busy_since, &timer) {
        (Some(since), Some(row)) => row.state == "Busy" && row.sequence_nr == since,
        (None, None) => true,
        _ => false,
    };
    if !agrees {
        return Err((
            "a record's timer row is committed with its event",
            format!(
                "{id}: journal {} since {:?}, timer row {timer:?}",
                journal.status, journal.busy_since
            ),
        ));
    }
    Ok(())
}
