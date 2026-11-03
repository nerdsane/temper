//! The append path: one transaction per append, run to its answer on a task
//! of its own.
//!
//! An append's caller can stop waiting at any await: an attempt timeout, a
//! dropped request, a stopped actor. The write must not stop with it. Dropped
//! after its commit was published (during the checkpoint a commit can run, for
//! one), the write lands and nobody learns it; dropped between two autocommit
//! statements, half of it lands. So:
//!
//! - every append is one transaction: its events, its segment's end and its
//!   rows commit together or not at all;
//! - a task that owns the append's inputs runs it, and the caller awaits that
//!   task's answer: dropping the caller drops the wait, never the write;
//! - a timeout bounds only waiting, for a write permit and for the write lock
//!   (`BEGIN IMMEDIATE`). Once the transaction began it runs to its answer;
//! - when `COMMIT` answers an error, the journal says what it did (the first
//!   event's id proves it landed). Absence cannot prove rollback while a
//!   remote commit may still be running. Such an outcome stays unknown and
//!   is never retried.
//!
//! An append that ended in [`Attempt::NotWritten`] with a transient error is
//! retried: nothing of it landed. The store is not a simulation-visible crate,
//! so the task is a plain `tokio::spawn` on the caller's runtime.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use temper_runtime::persistence::{
    CommitRows, PersistenceAppend, PersistenceAppendResult, PersistenceEnvelope, PersistenceError,
    ScheduledActionRecord, storage_error,
};
use tracing::Instrument;

use super::TursoEventStore;
use super::append_config::append_max_attempts;
use super::commit_probe::CommitPoint;
use super::instrumentation::WriteConnection;
use super::write_gate::{GatePermit, WritePriority};
use crate::driver::Transaction;
use crate::metrics::record_turso_write_retry;
use crate::retry::{is_transient_write_error, retry_delay_ms};

mod entity_rows;
mod write;

use entity_rows::EntityRows;
use write::{write_append, write_batch};

/// One append, owned, so a task can run it whatever happens to its caller.
#[derive(Debug, Clone)]
pub(super) struct OwnedAppend {
    persistence_id: String,
    expected_sequence: u64,
    events: Vec<PersistenceEnvelope>,
    scheduled_actions: Vec<ScheduledActionRecord>,
    fired_scheduled_action: Option<String>,
    entity_rows: EntityRows,
}

impl OwnedAppend {
    /// `events` after `expected_sequence`, with the rows of `rows`: its
    /// scheduled actions, and the entity's query-plane, timer and vector
    /// rows. Turso keeps them all in the journal's transaction.
    pub(super) fn new(
        persistence_id: &str,
        expected_sequence: u64,
        events: &[PersistenceEnvelope],
        rows: CommitRows<'_>,
    ) -> Self {
        Self {
            persistence_id: persistence_id.to_string(),
            expected_sequence,
            events: events.to_vec(),
            scheduled_actions: rows.scheduled_actions.to_vec(),
            fired_scheduled_action: rows.fired_scheduled_action.map(str::to_string),
            entity_rows: EntityRows::new(&rows),
        }
    }
}

/// What one attempt did.
pub(super) enum Attempt<T> {
    /// It landed.
    Written(T),
    /// Nothing of it landed.
    NotWritten(PersistenceError),
    /// Its commit failed and the journal cannot yet establish its outcome.
    Unknown(PersistenceError),
}

fn transient(error: &PersistenceError) -> bool {
    matches!(error, PersistenceError::Storage(msg) if is_transient_write_error(msg))
}

impl TursoEventStore {
    /// Append `append`, retrying an attempt that wrote nothing and failed
    /// transiently (ADR-0056).
    pub(super) async fn append_owned(&self, append: OwnedAppend) -> Result<u64, PersistenceError> {
        if append.events.is_empty() {
            return Ok(append.expected_sequence);
        }
        let append = Arc::new(append);
        self.retry_attempts("turso.append", move |store, permit| {
            let append = Arc::clone(&append);
            async move { store.append_attempt(permit, &append).await }
        })
        .await
    }

    /// Append every journal of `appends` in one transaction, retrying as
    /// [`Self::append_owned`] does.
    pub(super) async fn append_batch_owned(
        &self,
        appends: Vec<PersistenceAppend>,
    ) -> Result<Vec<PersistenceAppendResult>, PersistenceError> {
        let mut seen = std::collections::BTreeSet::new();
        for append in &appends {
            if !seen.insert(append.persistence_id.as_str()) {
                return Err(PersistenceError::Storage(format!(
                    "duplicate persistence_id '{}' in append_batch",
                    append.persistence_id
                )));
            }
        }
        let appends = Arc::new(appends);
        self.retry_attempts("turso.append_batch", move |store, permit| {
            let appends = Arc::clone(&appends);
            async move { store.append_batch_attempt(permit, &appends).await }
        })
        .await
    }

    /// Run attempts of `attempt` until one lands, one fails for good, or the
    /// attempts run out. Each attempt waits for the write gate's permit at
    /// high priority, and then runs on its own task, which holds it.
    async fn retry_attempts<T, F, Fut>(
        &self,
        operation: &'static str,
        attempt: F,
    ) -> Result<T, PersistenceError>
    where
        T: Send + 'static,
        F: Fn(TursoEventStore, GatePermit) -> Fut,
        Fut: Future<Output = Attempt<T>> + Send + 'static,
    {
        let total_attempts = append_max_attempts();
        let mut last_err: Option<PersistenceError> = None;
        for n in 0..total_attempts {
            if n > 0 {
                tokio::time::sleep(Duration::from_millis(retry_delay_ms(n - 1))).await;
            }
            let permit = self
                .acquire_write_permit(operation, WritePriority::High)
                .await?;
            match self.run_to_answer(attempt(self.clone(), permit)).await {
                Attempt::Written(value) => {
                    if n > 0 {
                        record_turso_write_retry(operation, n as u64, "succeeded");
                    }
                    return Ok(value);
                }
                Attempt::NotWritten(error) if transient(&error) => last_err = Some(error),
                Attempt::NotWritten(error) | Attempt::Unknown(error) => return Err(error),
            }
        }
        record_turso_write_retry(operation, total_attempts as u64, "exhausted");
        Err(last_err.expect("retry loop captured at least one error"))
    }

    /// Run `attempt` on a task of its own and wait for its answer. Dropping
    /// this future drops only the wait.
    async fn run_to_answer<T: Send + 'static>(
        &self,
        attempt: impl Future<Output = Attempt<T>> + Send + 'static,
    ) -> Attempt<T> {
        let task = tokio::spawn(attempt.instrument(tracing::Span::current()));
        match task.await {
            Ok(answer) => answer,
            Err(error) => Attempt::Unknown(PersistenceError::OutcomeUnknown(format!(
                "the append task ended without an answer: {error}"
            ))),
        }
    }

    /// Wait at most the attempt timeout for the write lock. Waiting is all
    /// this bounds: a transaction that began runs to its end.
    async fn begin_bounded<'c>(
        &self,
        conn: &'c WriteConnection,
        operation: &str,
    ) -> Result<Transaction<'c>, PersistenceError> {
        let timeout = self.append_attempt_timeout();
        match tokio::time::timeout(timeout, conn.begin_immediate()).await {
            Ok(Ok(tx)) => Ok(tx),
            Ok(Err(error)) => Err(storage_error(error)),
            Err(_) => Err(PersistenceError::Storage(format!(
                "{operation} waited more than {}ms for the write lock",
                timeout.as_millis()
            ))),
        }
    }

    /// One attempt of one append, on the write gate's `permit`.
    async fn append_attempt(&self, permit: GatePermit, append: &OwnedAppend) -> Attempt<u64> {
        let conn = match self.write_connection_on(permit).await {
            Ok(conn) => conn,
            Err(error) => return Attempt::NotWritten(error),
        };
        let tx = match self.begin_bounded(&conn, "turso.append").await {
            Ok(tx) => tx,
            Err(error) => return Attempt::NotWritten(error),
        };
        self.commit_point(CommitPoint::Began).await;
        let new_seq = match write_append(&tx, append).await {
            Ok(new_seq) => new_seq,
            Err(error) => {
                let _ = tx.rollback().await;
                return Attempt::NotWritten(error);
            }
        };
        self.commit_point(CommitPoint::Wrote).await;
        let committed = tx.commit().await;
        drop(conn);
        match committed {
            Ok(()) => {
                self.commit_point(CommitPoint::Committed).await;
                Attempt::Written(new_seq)
            }
            Err(error) => {
                let landed = self
                    .journal_holds(
                        &append.persistence_id,
                        append.expected_sequence,
                        &append.events[0],
                        storage_error(error),
                    )
                    .await;
                match landed {
                    Attempt::Written(()) => Attempt::Written(new_seq),
                    Attempt::NotWritten(error) => Attempt::NotWritten(error),
                    Attempt::Unknown(error) => Attempt::Unknown(error),
                }
            }
        }
    }

    /// One attempt of one batch, on the write gate's `permit`.
    async fn append_batch_attempt(
        &self,
        permit: GatePermit,
        appends: &[PersistenceAppend],
    ) -> Attempt<Vec<PersistenceAppendResult>> {
        let conn = match self.write_connection_on(permit).await {
            Ok(conn) => conn,
            Err(error) => return Attempt::NotWritten(error),
        };
        let tx = match self.begin_bounded(&conn, "turso.append_batch").await {
            Ok(tx) => tx,
            Err(error) => return Attempt::NotWritten(error),
        };
        let results = match write_batch(&tx, appends).await {
            Ok(results) => results,
            Err(error) => {
                let _ = tx.rollback().await;
                return Attempt::NotWritten(error);
            }
        };
        let committed = tx.commit().await;
        drop(conn);
        match committed {
            Ok(()) => Attempt::Written(results),
            Err(error) => {
                // The batch is one transaction: its first event says for all.
                let Some((append, first)) = appends
                    .iter()
                    .find_map(|append| append.events.first().map(|event| (append, event)))
                else {
                    return Attempt::Written(results);
                };
                match self
                    .journal_holds(
                        &append.persistence_id,
                        append.expected_sequence,
                        first,
                        storage_error(error),
                    )
                    .await
                {
                    Attempt::Written(()) => Attempt::Written(results),
                    Attempt::NotWritten(error) => Attempt::NotWritten(error),
                    Attempt::Unknown(error) => Attempt::Unknown(error),
                }
            }
        }
    }

    /// After a `COMMIT` answered `error`: whether the journal holds `event`
    /// after `from_sequence`.
    async fn journal_holds(
        &self,
        persistence_id: &str,
        from_sequence: u64,
        event: &PersistenceEnvelope,
        error: PersistenceError,
    ) -> Attempt<()> {
        use temper_runtime::persistence::EventStore;
        match self.read_events(persistence_id, from_sequence).await {
            Ok(events)
                if events
                    .iter()
                    .any(|stored| stored.metadata.event_id == event.metadata.event_id) =>
            {
                tracing::warn!(
                    persistence_id,
                    error = %error,
                    "turso commit answered an error and the journal holds its event: landed"
                );
                Attempt::Written(())
            }
            Ok(_) => Attempt::Unknown(PersistenceError::OutcomeUnknown(format!(
                "the commit failed ({error}); its event is absent, but the transaction may still finish"
            ))),
            Err(read_error) => Attempt::Unknown(PersistenceError::OutcomeUnknown(format!(
                "the commit failed ({error}) and the journal could not be read ({read_error})"
            ))),
        }
    }
}
