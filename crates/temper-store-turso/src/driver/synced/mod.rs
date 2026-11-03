//! A local database file whose committed changes are copied to its own
//! Turso Cloud database (Turso Sync).
//!
//! Reads and writes stay on the local file. [`SyncCopy::push_committed`]
//! sends every change committed before the call to the remote database in one
//! request, and returns only once the remote has committed it. Callers that
//! answer a request after a write await it first, so a confirmed change is
//! never only on this machine. A fresh machine with no local file starts from
//! the remote copy. That restore runs as long as it keeps writing the file
//! (see `restore`), and one stopped part way is kept aside and run again at
//! the next start.
//!
//! Writes are confirmed only in [`CopyState::Copying`]. A start that cannot
//! reach the remote serves reads and waits ([`CopyState::Unchecked`]); a file
//! found behind the remote is restored from it at the next start
//! ([`CopyState::Behind`]); a remote that refused a change stops confirmation
//! until an operator rebuilds it ([`CopyState::Stopped`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use temper_runtime::persistence::{PersistenceError, storage_error};

use super::{Connection, DriverError, DriverHandle, Lease};

mod checkpoint;
mod files;
mod pool;
mod push;
mod remote;
mod restore;
mod trim;
use files::{DIVERGED_SUFFIX, check_local_files, sidecar};
pub(crate) use pool::ConnectionPool;
use remote::Remote;
pub(crate) use trim::Writers as TrimWriters;

/// Whether writes to the file can be confirmed against its remote copy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyState {
    /// Every write is copied before it is confirmed.
    Copying,
    /// The remote could not be reached to check the file since this start.
    /// Reads are served; nothing is pushed or confirmed until the check passes.
    Unchecked,
    /// The remote holds changes from this file's history that the file lacks:
    /// it came back from an older copy, or a second server shares it. The
    /// next start restores the file from the remote.
    Behind,
    /// The remote refused a change and committed the rest of its push, so it
    /// is missing one. Kept across restarts until an operator rebuilds the
    /// remote and clears the stop ([`SyncCopy::clear_stop`]).
    Stopped(String),
}

/// The remote copy of one local database, shared by every connection to it.
#[derive(Clone)]
pub struct SyncCopy {
    inner: Arc<Inner>,
}

struct Inner {
    db: turso::sync::Database,
    path: PathBuf,
    remote: Remote,
    /// Every call into the sync engine (connect, push, stats, checkpoint) goes
    /// through this lock. The engine guards itself with a blocking mutex that
    /// a push holds across its network wait, so an unguarded `connect` blocks
    /// a runtime thread until the push ends, and on a one-thread runtime
    /// never returns.
    engine: tokio::sync::Mutex<()>,
    /// One push at a time. Writers that finish while a push is in flight wait
    /// here, not on `engine`, so a connection being opened waits for at most
    /// the push in flight, and the next push carries all of their changes.
    pushing: tokio::sync::Mutex<()>,
    /// Connections are reused, so store calls (reads above all) do not wait
    /// on that lock behind a push. The engine opens a connection by setting
    /// change capture on it, which also fails while another connection
    /// writes; reuse makes that rare and `connect` retries it.
    pool: ConnectionPool,
    /// Every connection in use holds this shared. While the log is reset,
    /// every statement on any connection fails at once with "database is
    /// locked", so a checkpoint takes it exclusively, waiting at most
    /// [`CHECKPOINT_TURN_BUDGET`]: longer could deadlock a store call that
    /// holds one connection while opening another.
    in_use: Arc<tokio::sync::RwLock<()>>,
    /// The newest change a successful push carried (from `last_change`).
    pushed: AtomicI64,
    /// The newest change in the file when it was opened, before this start
    /// committed anything. A file is behind when the remote has more.
    opened_at: AtomicI64,
    state: std::sync::Mutex<CopyState>,
    /// Push attempts that ran out of time in a row.
    timeouts: AtomicU32,
    /// Whether the remote may hold a push the engine never heard back from.
    unsure: AtomicBool,
    /// What the next trim batch deletes, planned from what the last one cost.
    trim_plan: std::sync::Mutex<trim::Plan>,
    /// Time added to every trim batch (see `simulate_slow_trim`).
    #[cfg(feature = "test-support")]
    simulated_trim: std::sync::Mutex<trim::SimulatedTrim>,
}

/// The sync engine's text for a statement the remote refused.
const REMOTE_STATEMENT_FAILED: &str = "failed to execute sql";

/// One push attempt. The engine's HTTP client has no timeout of its own.
const PUSH_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);

impl std::fmt::Debug for SyncCopy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncCopy").finish_non_exhaustive()
    }
}

impl SyncCopy {
    /// Open `path` as a synced database, restoring it from `remote_url` when
    /// there is no local database yet.
    ///
    /// Before anything commits, the file is checked against the remote's
    /// record of it. A file behind the remote (it came back from an older
    /// copy) is moved aside and the database restored from the remote, which
    /// holds everything the older copy did. A remote that cannot be reached
    /// leaves the copy [`CopyState::Unchecked`] until [`Self::recheck`] passes.
    /// A file that a stopped restore left part way is moved aside the same
    /// way, and restored again.
    pub(crate) async fn open(
        path: &str,
        remote_url: &str,
        auth_token: &str,
    ) -> Result<Self, DriverError> {
        let path = Path::new(path);
        check_local_files(path)?;
        let copy = Self::build(path, remote_url, auth_token).await?;
        match copy.recheck_inner().await {
            Ok(CopyState::Behind) => {}
            Ok(_) => return Ok(copy),
            Err(error) => {
                tracing::warn!(%error, "turso.sync remote unreachable at start; serving reads only until it answers");
                files::mark_unchecked(path, copy.inner.opened_at.load(Ordering::Acquire))?;
                return Ok(copy);
            }
        }
        drop(copy);
        let moved = files::move_aside(path)?;
        tracing::warn!(
            moved = %moved.display(),
            "turso.sync local file is behind its remote copy; restoring from the remote"
        );
        let copy = Self::build(path, remote_url, auth_token).await?;
        if let Err(error) = copy.recheck_inner().await {
            tracing::warn!(%error, "turso.sync check after the restore failed");
        }
        Ok(copy)
    }

    async fn build(path: &Path, remote_url: &str, auth_token: &str) -> Result<Self, DriverError> {
        // The engine's HTTPS client takes the process's rustls crypto provider,
        // and rustls cannot choose one when the build carries two (the whole
        // workspace does): the engine's IO thread then panics and every sync
        // call waits forever. An error here means one is already installed.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
        // Without sync metadata the engine restores the file from the remote,
        // and writes the metadata only once every page is on disk. Until then
        // the file is marked, so a start that stops part way is not taken
        // for a database that was never synced.
        if !sidecar(path, "-info").exists() {
            files::mark_restoring(path)?;
        }
        let local = path.to_string_lossy();
        let opening = turso::sync::Builder::new_remote(&local)
            .with_remote_url(remote_url)
            .with_auth_token(auth_token)
            .with_client_name("temper")
            .bootstrap_if_empty(true)
            .build();
        let db = restore::while_progressing(opening, || restore::footprint(path))
            .await
            .ok_or_else(|| {
                DriverError::SyncSetup(format!(
                    "opening {} with its remote copy made no progress for {} s",
                    path.display(),
                    restore::RESTORE_STALL_BUDGET.as_secs()
                ))
            })??;
        files::restored(path);
        let state = match std::fs::read_to_string(sidecar(path, DIVERGED_SUFFIX)) {
            Ok(cause) => CopyState::Stopped(cause),
            Err(_) => CopyState::Unchecked,
        };
        let client_id = remote::client_id(&files::sync_metadata(path)?);
        let copy = Self {
            inner: Arc::new(Inner {
                db,
                path: path.to_path_buf(),
                remote: Remote::new(remote_url, auth_token, client_id),
                engine: tokio::sync::Mutex::new(()),
                pushing: tokio::sync::Mutex::new(()),
                pushed: AtomicI64::new(0),
                opened_at: AtomicI64::new(0),
                pool: ConnectionPool::default(),
                in_use: Arc::new(tokio::sync::RwLock::new(())),
                state: std::sync::Mutex::new(state),
                timeouts: AtomicU32::new(0),
                unsure: AtomicBool::new(true),
                trim_plan: std::sync::Mutex::new(trim::FIRST_PLAN),
                #[cfg(feature = "test-support")]
                simulated_trim: std::sync::Mutex::new(trim::SimulatedTrim::default()),
            }),
        };
        // Change numbers follow commit order only with one writer at a time
        // (WAL). The engine can switch a file to MVCC for an MVCC remote.
        let mode = copy.query_text("PRAGMA journal_mode").await?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(DriverError::SyncSetup(format!(
                "{} is in journal mode {mode}; Temper's Turso Sync copy needs WAL",
                path.display()
            )));
        }
        let opened_at = match files::unchecked_since(path) {
            Some(opened_at) => opened_at,
            None => copy
                .last_change()
                .await
                .map_err(|error| DriverError::SyncSetup(error.to_string()))?,
        };
        copy.inner.opened_at.store(opened_at, Ordering::Release);
        let mut warm = Vec::with_capacity(WARM_CONNECTIONS);
        for _ in 0..WARM_CONNECTIONS {
            warm.push(copy.connect().await?);
        }
        drop(warm);
        Ok(copy)
    }

    /// Whether writes can be confirmed now, and if not, why.
    pub fn state(&self) -> CopyState {
        self.inner.state.lock().unwrap().clone()
    }

    /// Push attempts that ran out of time in a row. The engine sends its
    /// requests one at a time, so a request the remote never answers holds
    /// every later push until its connection fails; a restart clears it.
    pub fn pushes_timed_out_in_a_row(&self) -> u32 {
        self.inner.timeouts.load(Ordering::Acquire)
    }

    /// Record for monitoring whether writes are paused: the gauge
    /// `temper_turso_sync_writes_paused` is 1 in every state but
    /// [`CopyState::Copying`]. The reason is in the log line of the change.
    pub fn record_state(&self) {
        static PAUSED: std::sync::OnceLock<opentelemetry::metrics::Gauge<u64>> =
            std::sync::OnceLock::new();
        let gauge = PAUSED.get_or_init(|| {
            opentelemetry::global::meter("temper-store-turso")
                .u64_gauge("temper_turso_sync_writes_paused")
                .with_description("1 while a synced database cannot confirm writes.")
                .build()
        });
        gauge.record(u64::from(self.state() != CopyState::Copying), &[]);
    }

    /// Check an [`CopyState::Unchecked`] file against the remote and return
    /// the state after it. Other states are returned as they are.
    pub async fn recheck(&self) -> Result<CopyState, PersistenceError> {
        self.recheck_inner().await.map_err(storage_error)
    }

    async fn recheck_inner(&self) -> Result<CopyState, DriverError> {
        if self.state() != CopyState::Unchecked {
            return Ok(self.state());
        }
        let remote_has = self.inner.remote.last_change().await?;
        let opened_at = self.inner.opened_at.load(Ordering::Acquire);
        if remote_has > opened_at {
            self.enter(CopyState::Behind, &format!(
                "the remote has changes up to {remote_has} from this file's history, the file only \
                 up to {opened_at}"
            ));
        } else {
            self.enter(
                CopyState::Copying,
                "the file checks out against its remote copy",
            );
        }
        Ok(self.state())
    }

    /// Move to `next` unless the copy is already stopped or behind, which only
    /// a restart or an operator clears.
    pub(super) fn enter(&self, next: CopyState, why: &str) {
        let mut state = self.inner.state.lock().unwrap();
        if matches!(*state, CopyState::Stopped(_) | CopyState::Behind) || *state == next {
            return;
        }
        *state = next.clone();
        drop(state);
        match &next {
            CopyState::Copying => {
                files::mark_checked(&self.inner.path);
                tracing::info!(why, "turso.sync writes are confirmed again");
            }
            CopyState::Unchecked => {}
            CopyState::Behind | CopyState::Stopped(_) => tracing::error!(
                event = "turso.sync.writes_stopped",
                state = ?next,
                why,
                "turso.sync stopped confirming writes"
            ),
        }
        if let CopyState::Stopped(cause) = &next
            && let Err(error) = std::fs::write(sidecar(&self.inner.path, DIVERGED_SUFFIX), cause)
        {
            tracing::error!(%error, "turso.sync could not record the stop");
        }
    }

    pub(super) async fn connect(&self) -> Result<Connection, DriverError> {
        let in_use = Some(self.inner.in_use.clone().read_owned().await);
        let pool = Some(self.inner.pool.clone());
        if let Some(conn) = self.inner.pool.take() {
            let conn = Lease::new(conn, in_use);
            return Ok(Connection::Local { conn, pool });
        }
        let started = Instant::now();
        let mut delay = Duration::from_millis(2);
        loop {
            let opened = {
                let _engine = self.inner.engine.lock().await;
                self.inner.db.connect().await
            };
            match opened {
                Ok(conn) => {
                    conn.busy_timeout(Duration::from_secs(5))?;
                    // A commit does not wait for the disk: NORMAL flushes the
                    // file at each checkpoint instead of each commit. Every
                    // confirmed write is already on the remote, so a machine
                    // crash that rolls the file back to its last checkpoint
                    // loses only writes answered "not confirmed", and the next
                    // start restores a file behind its remote. Unsynced stores
                    // keep Turso's FULL. Rita's choice on 27 Oct 2026: on a
                    // Tensorlake drive FULL fell behind at 20 moves a second.
                    conn.execute("PRAGMA synchronous = NORMAL", ()).await?;
                    let conn = Lease::new(Arc::new(DriverHandle::new(conn)), in_use);
                    return Ok(Connection::Local { conn, pool });
                }
                Err(error) if is_busy(&error) && started.elapsed() < CONNECT_BUSY_BUDGET => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_millis(100));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// With the app stopped: clear a [`CopyState::Stopped`] copy once its
    /// remote has been rebuilt from the file. Returns the cause it cleared.
    pub fn clear_stop(path: &str) -> Result<Option<String>, PersistenceError> {
        files::clear_stop(Path::new(path.strip_prefix("file:").unwrap_or(path)))
            .map_err(storage_error)
    }

    /// With the app stopped: move the file and its sync files aside, so the
    /// next start restores the database from the remote. Returns where they went.
    pub fn set_aside(path: &str) -> Result<PathBuf, PersistenceError> {
        files::move_aside(Path::new(path.strip_prefix("file:").unwrap_or(path)))
            .map_err(storage_error)
    }

    /// With the app stopped: download the remote copy into `path`, which must
    /// not exist yet, the way a start restores it, without serving, pushing or
    /// changing the remote. Returns [`Self::describe`] of the result.
    pub async fn fetch(
        path: &str,
        remote_url: &str,
        auth_token: &str,
    ) -> Result<String, PersistenceError> {
        let local = path.strip_prefix("file:").unwrap_or(path);
        files::refuse_existing(Path::new(local)).map_err(storage_error)?;
        let copy = Self::open(local, remote_url, auth_token)
            .await
            .map_err(storage_error)?;
        drop(copy);
        Self::describe(local)
    }

    /// How far a restore into `path` has got: the bytes written so far, and
    /// the database's whole size once the remote has sent it.
    pub fn restored_bytes(path: &str) -> (u64, u64) {
        restore::written(Path::new(path.strip_prefix("file:").unwrap_or(path)))
    }

    /// What is on disk for `path`: the stop, if any, and how far the file and
    /// its last push got. For an operator; the app may be running.
    pub fn describe(path: &str) -> Result<String, PersistenceError> {
        files::describe(Path::new(path.strip_prefix("file:").unwrap_or(path)))
            .map_err(storage_error)
    }

    /// Run one statement on the local file and return the rows it changed.
    /// For tests that need a state no store call makes, such as a change log
    /// longer than one push can carry; never built into the server.
    #[cfg(feature = "test-support")]
    pub async fn execute_local(&self, sql: &str) -> Result<u64, PersistenceError> {
        let conn = self.connect().await.map_err(storage_error)?;
        conn.execute(sql, ()).await.map_err(storage_error)
    }
}

/// How long a checkpoint waits for the calls in flight before it gives way.
/// Every new call waits behind it meanwhile, so it stays well inside the
/// 10 s a request may wait for its answer; and a write holds its connection
/// for one commit, about 1 s at worst on a Tensorlake drive (fsync p99 on the
/// staging canary, 26 Oct 2026).
const CHECKPOINT_TURN_BUDGET: Duration = Duration::from_secs(1);

/// How long opening a connection retries while other connections write; as
/// long as the store's statements wait for a busy database.
const CONNECT_BUSY_BUDGET: Duration = Duration::from_secs(30);

/// Connections opened at start and kept idle, so a burst of requests finds
/// them instead of opening new ones while writes hold the database.
const WARM_CONNECTIONS: usize = 16;

fn is_busy(error: &turso::Error) -> bool {
    matches!(error, turso::Error::Busy(_))
        || error
            .to_string()
            .to_ascii_lowercase()
            .contains("database is busy")
}
