//! Pushing committed changes to the remote, and what a refused push means.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use temper_runtime::persistence::{PersistenceError, storage_error};
use tracing::Instrument;
use tracing::field::Empty;

use super::{
    CopyState, DriverError, PUSH_ATTEMPT_TIMEOUT, REMOTE_STATEMENT_FAILED, SyncCopy, files,
};

/// How long the remote may take to say how far it got, under the 10 s its
/// HTTP client allows, so a hung remote is told from a refusing one.
const REMOTE_READ_TIMEOUT: Duration = Duration::from_secs(8);

/// How long a push may take before a fresh request to the remote is timed
/// beside it. A push is two requests to the remote, about 70 ms each on a new
/// connection and less on the engine's kept one; one lost DNS answer adds 1 s
/// (temper-cli's `RES_OPTIONS`). A push still waiting at 2 s is past both.
const SLOW_PUSH: Duration = Duration::from_secs(2);

impl SyncCopy {
    /// Copy every change committed before this call to the remote database.
    ///
    /// Returns without a network request when another push already carried
    /// those changes, so writers that commit together share one push. An
    /// error means some of them may be only local; they stay in the local
    /// change log and the next successful push sends them. Nothing is pushed
    /// unless the copy is [`CopyState::Copying`]. It never trims the change
    /// log, so an answer waits only for its own changes; the background loop
    /// trims (`TursoEventStore::trim_change_log`).
    pub async fn push_committed(&self) -> Result<(), PersistenceError> {
        // Changes are numbered in commit order (one writer at a time), so
        // everything this caller committed is at or below `wanted`.
        let wanted = self.last_change().await?;
        self.require_copying()?;
        // Already on the remote: nothing to wait for, not even a push that
        // holds the lock for later changes.
        if self.inner.pushed.load(Ordering::Acquire) >= wanted {
            return Ok(());
        }
        // The engine logs each step of a push; in this span they reach the
        // trace with their times, so a slow push shows which step waited. It
        // opens before the lock, so the time spent queued behind another push
        // is in it too (`waited_ms`).
        let span = tracing::info_span!(
            "turso.sync.push",
            waited_ms = Empty,
            carried = Empty,
            changes = Empty,
            elapsed_ms = Empty,
            error = Empty
        );
        self.push_through(wanted, &span)
            .instrument(span.clone())
            .await
    }

    /// Wait for the push lock, then push unless the push that held it
    /// carried everything up to `wanted` (`carried`).
    async fn push_through(
        &self,
        wanted: i64,
        span: &tracing::Span,
    ) -> Result<(), PersistenceError> {
        let asked = Instant::now();
        let _pushing = self.inner.pushing.lock().await;
        span.record("waited_ms", asked.elapsed().as_millis() as u64);
        self.require_copying()?;
        let carried = self.inner.pushed.load(Ordering::Acquire) >= wanted;
        span.record("carried", carried);
        if carried {
            return Ok(());
        }
        let started = Instant::now();
        let mut push = std::pin::pin!(self.push_now());
        let pushed = match tokio::time::timeout(SLOW_PUSH, &mut push).await {
            Ok(pushed) => pushed,
            Err(_) => {
                self.time_a_fresh_request(span);
                push.await
            }
        };
        span.record("elapsed_ms", started.elapsed().as_millis() as u64);
        if let Err(error) = &pushed {
            span.record("error", tracing::field::display(error));
        }
        pushed
    }

    /// Time a fresh request to the remote beside a slow push, and log what
    /// it found: a slow lookup is DNS, a slow connection is the network, a
    /// slow answer is the remote, and all fast while the push still waits
    /// is the engine's own connection or lookup. Spawned, so the push's
    /// answer never waits for it.
    fn time_a_fresh_request(&self, push: &tracing::Span) {
        let inner = Arc::clone(&self.inner);
        let timed = async move {
            let fresh = inner.remote.time_fresh_request().await;
            tracing::warn!(
                remote = fresh.host.as_str(),
                lookup_ms = fresh.lookup_ms,
                connect_ms = fresh.connect_ms,
                answer_ms = fresh.answer_ms,
                status = fresh.status,
                failed = fresh.failed.as_deref(),
                "turso.sync push slow; a fresh request to the remote, timed step by step"
            );
        };
        tokio::spawn(timed.instrument(push.clone()));
    }

    async fn push_now(&self) -> Result<(), PersistenceError> {
        // The push reads the change log after this, so it carries `covered`.
        let covered = self.last_change().await?;
        tracing::Span::current().record(
            "changes",
            covered - self.inner.pushed.load(Ordering::Acquire),
        );
        let remote_had = self.remote_had().await?;
        // Until it is known to have succeeded, a push may have committed on
        // the remote past the engine's own record: set before it starts, so
        // a caller that gives up on it (an answer's time limit) leaves it set.
        self.inner.unsure.store(true, Ordering::Release);
        let pushed = {
            let _engine = self.inner.engine.lock().await;
            tokio::time::timeout(PUSH_ATTEMPT_TIMEOUT, self.inner.db.push()).await
        };
        match pushed {
            Ok(Ok(())) => {
                self.inner.unsure.store(false, Ordering::Release);
                self.inner.timeouts.store(0, Ordering::Release);
            }
            Ok(Err(error)) => {
                self.inner.timeouts.store(0, Ordering::Release);
                let message = error.to_string();
                if message.contains(REMOTE_STATEMENT_FAILED) {
                    self.after_refusal(remote_had, &message).await;
                }
                return Err(storage_error(error));
            }
            Err(_) => {
                self.inner.timeouts.fetch_add(1, Ordering::AcqRel);
                return Err(PersistenceError::Storage(format!(
                    "turso.sync push timed out after {} s",
                    PUSH_ATTEMPT_TIMEOUT.as_secs()
                )));
            }
        }
        // The engine sends only changes numbered above the remote's record of
        // this file's last push. A record above everything the file holds
        // means the remote silently skipped what was just written: a second
        // server is pushing as this file.
        let remote_has = self.last_pushed_hint().await.map_err(storage_error)?;
        let local_has = self.last_change().await?;
        if remote_has > local_has {
            self.enter(CopyState::Behind, &format!(
                "after a push the remote has changes up to {remote_has}, the file only up to {local_has}"
            ));
            return self.require_copying();
        }
        self.inner.pushed.fetch_max(covered, Ordering::AcqRel);
        Ok(())
    }

    /// The remote's record of this file's last push before the next one.
    /// The engine's own record is behind it when a push it never heard back
    /// from committed (it timed out, its answer was lost, or this server
    /// restarted after one), so then, and first after every start, the
    /// remote is asked.
    async fn remote_had(&self) -> Result<i64, PersistenceError> {
        let hint = self.last_pushed_hint().await.map_err(storage_error)?;
        if !self.inner.unsure.load(Ordering::Acquire) {
            return Ok(hint);
        }
        // A remote that does not answer this counts like a push that timed
        // out, so a hung remote still ends in the restart.
        match tokio::time::timeout(REMOTE_READ_TIMEOUT, self.inner.remote.last_change()).await {
            Ok(read) => Ok(hint.max(read.map_err(storage_error)?)),
            Err(_) => {
                self.inner.timeouts.fetch_add(1, Ordering::AcqRel);
                Err(PersistenceError::Storage(format!(
                    "turso.sync the remote did not say how far it got within {} s",
                    REMOTE_READ_TIMEOUT.as_secs()
                )))
            }
        }
    }

    /// Whether the next push first asks the remote how far it got, because
    /// a push since this start may have committed there unheard.
    pub fn remote_record_unsure(&self) -> bool {
        self.inner.unsure.load(Ordering::Acquire)
    }

    /// A push the remote answered with a statement error. The remote skips a
    /// statement that fails and commits the rest of the push together with
    /// its record of the push, so a retry would succeed with that change
    /// missing. Its record moving past where it was shows that happened; a
    /// refusal that committed nothing (a busy remote) leaves it in place.
    async fn after_refusal(&self, remote_had: i64, message: &str) {
        match self.inner.remote.last_change().await {
            Ok(remote_has) if remote_has <= remote_had => {
                tracing::warn!(
                    cause = message,
                    "turso.sync push refused without committing; will retry"
                );
            }
            Ok(_) => self.enter(
                CopyState::Stopped(message.to_string()),
                "the remote refused a change and committed the rest",
            ),
            Err(error) => self.enter(
                CopyState::Stopped(format!(
                    "{message} (and its progress could not be read: {error})"
                )),
                "the remote refused a change and its progress could not be read",
            ),
        }
    }

    pub(super) fn require_copying(&self) -> Result<(), PersistenceError> {
        match self.state() {
            CopyState::Copying => Ok(()),
            CopyState::Unchecked => Err(PersistenceError::Storage(
                "turso.sync the remote copy has not been reachable since this start".to_string(),
            )),
            CopyState::Behind => Err(PersistenceError::Storage(
                "turso.sync the file is behind its remote copy; it is restored at the next start"
                    .to_string(),
            )),
            CopyState::Stopped(cause) => Err(PersistenceError::Storage(format!(
                "turso.sync the remote copy is missing a change ({cause})"
            ))),
        }
    }

    /// The remote's record of this file's last pushed change, as the engine
    /// saved it after its last successful push.
    pub(super) async fn last_pushed_hint(&self) -> Result<i64, DriverError> {
        let info = files::read_sync_metadata(&self.inner.path).await?;
        Ok(info["last_pushed_change_id_hint"].as_i64().unwrap_or(0))
    }

    /// How many rows the local change log holds.
    pub async fn change_log_rows(&self) -> Result<i64, PersistenceError> {
        let conn = self.connect().await.map_err(storage_error)?;
        let mut rows = conn
            .query("SELECT COUNT(*) FROM turso_cdc", ())
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(0);
        };
        row.get::<i64>(0).map_err(storage_error)
    }

    /// Bytes of row images the local change log holds for changes numbered
    /// above `after`.
    pub async fn change_log_bytes_after(&self, after: i64) -> Result<i64, PersistenceError> {
        let conn = self.connect().await.map_err(storage_error)?;
        let mut rows = conn
            .query(
                "SELECT COALESCE(SUM(COALESCE(LENGTH(before), 0) + COALESCE(LENGTH(after), 0) \
                 + COALESCE(LENGTH(updates), 0)), 0) FROM turso_cdc WHERE change_id > ?1",
                crate::driver::params![after],
            )
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(0);
        };
        row.get::<i64>(0).map_err(storage_error)
    }

    /// The number of the newest committed change in the local change log.
    pub async fn last_change(&self) -> Result<i64, PersistenceError> {
        let conn = self.connect().await.map_err(storage_error)?;
        let mut rows = conn
            .query("SELECT MAX(change_id) FROM turso_cdc", ())
            .await
            .map_err(storage_error)?;
        let Some(row) = rows.next().await.map_err(storage_error)? else {
            return Ok(0);
        };
        Ok(row
            .get::<Option<i64>>(0)
            .map_err(storage_error)?
            .unwrap_or(0))
    }

    pub(super) async fn query_text(&self, sql: &str) -> Result<String, DriverError> {
        let conn = self.connect().await?;
        let mut rows = conn.query(sql, ()).await?;
        let Some(row) = rows.next().await? else {
            return Ok(String::new());
        };
        Ok(row.get::<String>(0).unwrap_or_default())
    }

    /// [`Self::push_committed`], retried with backoff until it succeeds or
    /// `budget` has passed. The remote records the last change it applied in
    /// the same transaction as the changes, so a retry after a push whose
    /// answer was lost does not apply anything twice.
    ///
    /// The budget covers the whole call, waiting behind other pushes included.
    pub async fn push_committed_within(&self, budget: Duration) -> Result<(), PersistenceError> {
        tokio::time::timeout(budget, self.push_committed_retrying(budget))
            .await
            .unwrap_or_else(|_| {
                Err(PersistenceError::Storage(format!(
                    "turso.sync push not done within {} ms",
                    budget.as_millis()
                )))
            })
    }

    async fn push_committed_retrying(&self, budget: Duration) -> Result<(), PersistenceError> {
        let started = Instant::now();
        let mut delay = Duration::from_millis(50);
        loop {
            match self.push_committed().await {
                Ok(()) => return Ok(()),
                Err(error)
                    if started.elapsed() + delay < budget && self.state() == CopyState::Copying =>
                {
                    tracing::warn!(%error, delay_ms = delay.as_millis() as u64, "turso.sync push failed; retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(2));
                }
                Err(error) => return Err(error),
            }
        }
    }
}
