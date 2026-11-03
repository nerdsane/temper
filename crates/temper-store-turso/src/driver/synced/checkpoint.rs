//! Folding the local write-ahead log back into the database file.

use std::time::{Duration, Instant};

use temper_runtime::persistence::{PersistenceError, storage_error};

use super::{CHECKPOINT_TURN_BUDGET, SyncCopy};

/// A checkpoint that waited for its turn and moved the log for this long in
/// all is logged: every call that needed a connection meanwhile waited as
/// long, the write holding the store's write gate included.
const SLOW_FOLD: Duration = Duration::from_millis(100);

impl SyncCopy {
    /// Move the local write-ahead log into the database file once it is larger
    /// than `max_wal_bytes`. Turso Sync turns automatic checkpoints off, so
    /// without this the log grows with every write.
    ///
    /// While the log is reset, any statement on any connection fails at once
    /// with "database is locked", so this takes a turn: the calls in flight
    /// finish and new ones wait behind it. Skipping whenever a call was in
    /// flight let the log grow without bound under steady load, and the
    /// checkpoint that finally ran moved all of it while every call waited.
    ///
    /// It is a plain TRUNCATE checkpoint, not the engine's. The engine's also
    /// copies the old version of every changed page into its revert log, which
    /// only a pull reads (`turso_sync_engine` 0.7.2 and 0.8.3,
    /// `database_sync_engine.rs` `checkpoint` and `apply_changes_internal`).
    /// This store never pulls: a restore sets the file aside and bootstraps.
    /// On a file bootstrapped from its remote that copy grows with every page
    /// changed since, and all of it ran inside the turn.
    pub async fn checkpoint_if_wal_over(
        &self,
        max_wal_bytes: u64,
    ) -> Result<bool, PersistenceError> {
        let _engine = self.inner.engine.lock().await;
        let stats = self.inner.db.stats().await.map_err(storage_error)?;
        if stats.main_wal_size < max_wal_bytes {
            return Ok(false);
        }
        // A call that holds one connection while it opens another cannot
        // finish while this waits, so the wait has a budget and the next tick
        // tries again.
        let started = Instant::now();
        let Ok(_idle) =
            tokio::time::timeout(CHECKPOINT_TURN_BUDGET, self.inner.in_use.write()).await
        else {
            tracing::warn!(
                budget_ms = CHECKPOINT_TURN_BUDGET.as_millis() as u64,
                wal_bytes = stats.main_wal_size,
                "turso.sync calls kept the file in use; the next tick checkpoints"
            );
            return Ok(false);
        };
        let waited = started.elapsed();
        let conn = self.inner.db.connect().await.map_err(storage_error)?;
        let mut rows = conn
            .query("PRAGMA wal_checkpoint(TRUNCATE)", ())
            .await
            .map_err(storage_error)?;
        let busy = match rows.next().await.map_err(storage_error)? {
            Some(row) => row.get::<i64>(0).map_err(storage_error)?,
            None => 1,
        };
        let folded = started.elapsed().saturating_sub(waited);
        if waited + folded >= SLOW_FOLD {
            tracing::warn!(
                wal_bytes = stats.main_wal_size,
                waited_ms = waited.as_millis() as u64,
                fold_ms = folded.as_millis() as u64,
                "turso.sync checkpoint held new calls"
            );
        }
        if busy != 0 {
            return Err(PersistenceError::Storage(
                "turso.sync checkpoint left the log in place: a connection outside the store was reading"
                    .to_string(),
            ));
        }
        Ok(true)
    }
}
