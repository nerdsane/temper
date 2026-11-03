use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use temper_runtime::persistence::PersistenceError;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::TursoEventStore;
use super::instrumentation::WriteConnection;
use crate::metrics::{record_turso_write_gate_hold, record_turso_write_gate_wait};

/// Which writes go first when several wait for the gate's permit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WritePriority {
    /// A write a move or a request waits on: journal appends, specs, app
    /// installs, policies, secrets.
    High,
    /// A write made behind the work it records: projections, trajectories,
    /// timers, invocation logs, blobs. It waits while any high-priority
    /// write waits.
    Low,
}

impl WritePriority {
    fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Low => "low",
        }
    }
}

/// How often a writer that gives way to others looks again. It delays only
/// the one looking, and is short against one commit on a slow drive (about
/// 170 ms on a Tensorlake drive, roll3, 26 Oct 2026).
const GIVE_WAY_RECHECK: Duration = Duration::from_millis(25);

/// A wait at the gate this long is logged, and so is a hold this long that
/// kept another writer waiting: the pair names who held the gate while others
/// queued, and the hold says how much of it went to getting a connection and
/// how much to the transaction. Metrics carry no Temper id in Datadog, so the
/// logs are what tell one Temper's queue from another's.
const SLOW_TURN: Duration = Duration::from_millis(100);

/// The process-local queue for writes to one database: its one permit, and
/// the writers waiting for it.
///
/// One, because the database has one writer: SQLite and Turso, local, synced
/// or remote, let one transaction write at a time. A second permit would not
/// let a second write run; it would move that write's wait from here, a
/// first-come queue with no deadline of its own, to the database's busy
/// handler, which retries every 1 to 100 ms (turso_core 0.7.2 `busy.rs`
/// 62-90), lets a newcomer in ahead of one that has waited longer, and
/// refuses a move whose wait passes its lock wait.
#[derive(Debug)]
pub(super) struct WriteGate {
    pub(super) permit: Arc<Semaphore>,
    /// High-priority writers waiting for the permit. Low-priority writers
    /// wait while there are any.
    high_waiting: AtomicUsize,
    /// Writers of either priority waiting for the permit. The trim waits
    /// while there are any.
    waiting: AtomicUsize,
}

impl WriteGate {
    pub(super) fn new() -> Self {
        Self {
            permit: Arc::new(Semaphore::new(1)),
            high_waiting: AtomicUsize::new(0),
            waiting: AtomicUsize::new(0),
        }
    }
}

/// The gate's permit, held by one writer. Dropping it frees the gate and
/// records how long the writer held it, which is how long every write that
/// arrived meanwhile waited for it.
#[derive(Debug)]
pub(crate) struct GatePermit {
    _permit: OwnedSemaphorePermit,
    gate: Arc<WriteGate>,
    operation: &'static str,
    acquired: Instant,
    /// When the writer's connection was ready. Getting one can wait behind a
    /// push or a checkpoint, so a slow hold is split at this point into that
    /// wait and the transaction.
    connected: Option<Instant>,
}

impl GatePermit {
    fn new(permit: OwnedSemaphorePermit, gate: &Arc<WriteGate>, operation: &'static str) -> Self {
        Self {
            _permit: permit,
            gate: Arc::clone(gate),
            operation,
            acquired: Instant::now(),
            connected: None,
        }
    }
}

impl Drop for GatePermit {
    fn drop(&mut self) {
        let held = self.acquired.elapsed();
        record_turso_write_gate_hold(held, self.operation);
        // The permit is still this writer's here: it is let go after `drop`.
        let waiting = self.gate.waiting.load(Ordering::Acquire);
        if held >= SLOW_TURN && waiting > 0 {
            let held_ms = held.as_millis() as u64;
            let connection_ms = self
                .connected
                .map_or(0, |at| at.duration_since(self.acquired).as_millis() as u64);
            tracing::warn!(
                operation = self.operation,
                held_ms,
                connection_ms,
                transaction_ms = held_ms.saturating_sub(connection_ms),
                waiting,
                "turso.write_gate held"
            );
        }
    }
}

impl TursoEventStore {
    /// A connection that may write, once the gate's permit is this writer's.
    /// Every write to the database goes through one.
    pub(crate) async fn write_connection(
        &self,
        operation: &'static str,
        priority: WritePriority,
    ) -> Result<WriteConnection, PersistenceError> {
        let permit = self.acquire_write_permit(operation, priority).await?;
        self.write_connection_on(permit).await
    }

    /// A connection that may write on a permit already taken.
    pub(super) async fn write_connection_on(
        &self,
        mut permit: GatePermit,
    ) -> Result<WriteConnection, PersistenceError> {
        let conn = self.configured_connection().await?;
        permit.connected = Some(Instant::now());
        Ok(WriteConnection::new(conn, permit))
    }

    /// Acquire the process-local Turso write lane.
    ///
    /// Remote Turso serializes writes at the database. Letting every actor open
    /// an immediate transaction concurrently turns ordinary write pressure into
    /// a lock/timeout storm. This gate makes the queue explicit in-process, so
    /// timeout budgets apply to the actual database operation instead of many
    /// requests racing for the same remote writer. Every journal append takes
    /// a high-priority permit, so new low-priority writes yield to it.
    pub(super) async fn acquire_write_permit(
        &self,
        operation: &'static str,
        priority: WritePriority,
    ) -> Result<GatePermit, PersistenceError> {
        let started = Instant::now();
        let permit = self.wait_for_permit(operation, priority, started).await;
        let outcome = if permit.is_ok() {
            "acquired"
        } else {
            "timed_out"
        };
        record_turso_write_gate_wait(started.elapsed(), operation, priority.as_str(), outcome);
        Ok(GatePermit::new(permit?, &self.write_gate, operation))
    }

    async fn wait_for_permit(
        &self,
        operation: &'static str,
        priority: WritePriority,
        started: Instant,
    ) -> Result<OwnedSemaphorePermit, PersistenceError> {
        let timeout = write_gate_wait_timeout();
        let waiting = Waiting::new(&self.write_gate, priority);

        if priority == WritePriority::Low {
            self.wait_for_high_priority_writers(operation, started, timeout)
                .await?;
        }

        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(write_gate_timeout_error(operation, priority, timeout));
        }

        let permit =
            tokio::time::timeout(remaining, self.write_gate.permit.clone().acquire_owned())
                .await
                .map_err(|_| write_gate_timeout_error(operation, priority, timeout))?
                .map_err(|_| write_gate_closed())?;
        drop(waiting);

        let waited = started.elapsed();
        if waited >= SLOW_TURN {
            tracing::warn!(
                operation,
                priority = priority.as_str(),
                wait_ms = waited.as_millis() as u64,
                "turso.write_gate waited"
            );
        } else {
            tracing::debug!(
                operation,
                priority = priority.as_str(),
                wait_ms = waited.as_millis() as u64,
                "turso.write_gate acquired"
            );
        }

        Ok(permit)
    }

    /// The write gate's permit, for one batch of the change log's trim
    /// ([`Self::trim_change_log`]).
    ///
    /// A batch must never keep a writer waiting, so it starts only when no
    /// writer waits at the gate, and it never queues there: a queued request
    /// would hold back writers that arrive behind it.
    async fn trim_turn(&self) -> Result<GatePermit, PersistenceError> {
        let gate = &self.write_gate;
        loop {
            if gate.waiting.load(Ordering::Acquire) == 0 {
                match gate.permit.clone().try_acquire_owned() {
                    Ok(turn) => {
                        return Ok(GatePermit::new(turn, gate, "turso.trim_change_log"));
                    }
                    Err(tokio::sync::TryAcquireError::Closed) => return Err(write_gate_closed()),
                    Err(tokio::sync::TryAcquireError::NoPermits) => {}
                }
            }
            tokio::time::sleep(GIVE_WAY_RECHECK).await;
        }
    }

    async fn wait_for_high_priority_writers(
        &self,
        operation: &'static str,
        started: Instant,
        timeout: Duration,
    ) -> Result<(), PersistenceError> {
        while self.write_gate.high_waiting.load(Ordering::Acquire) > 0 {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(write_gate_timeout_error(
                    operation,
                    WritePriority::Low,
                    timeout,
                ));
            }
            tokio::time::sleep(remaining.min(GIVE_WAY_RECHECK)).await;
        }
        Ok(())
    }
}

/// The write gate as the change log's trim sees it.
pub(super) struct TrimTurns<'a>(pub(super) &'a TursoEventStore);

impl crate::driver::TrimWriters for TrimTurns<'_> {
    type Turn = GatePermit;

    fn turn(
        &self,
    ) -> impl std::future::Future<Output = Result<Self::Turn, PersistenceError>> + Send {
        self.0.trim_turn()
    }

    fn waiting(&self) -> bool {
        self.0.write_gate.waiting.load(Ordering::Acquire) > 0
    }

    fn lock_wait(&self) -> Duration {
        self.0.append_attempt_timeout()
    }
}

/// Counts one writer as waiting at the gate until it has a permit, gives up,
/// or is dropped.
struct Waiting<'a> {
    gate: &'a WriteGate,
    priority: WritePriority,
}

impl<'a> Waiting<'a> {
    fn new(gate: &'a WriteGate, priority: WritePriority) -> Self {
        gate.waiting.fetch_add(1, Ordering::AcqRel);
        if priority == WritePriority::High {
            gate.high_waiting.fetch_add(1, Ordering::AcqRel);
        }
        Self { gate, priority }
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        if self.priority == WritePriority::High {
            self.gate.high_waiting.fetch_sub(1, Ordering::AcqRel);
        }
        self.gate.waiting.fetch_sub(1, Ordering::AcqRel);
    }
}

fn write_gate_wait_timeout() -> Duration {
    const DEFAULT_WRITE_GATE_WAIT_TIMEOUT_MS: u64 = 30_000;
    const MIN_WRITE_GATE_WAIT_TIMEOUT_MS: u64 = 100;

    let configured = std::env::var("TEMPER_TURSO_WRITE_GATE_WAIT_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(DEFAULT_WRITE_GATE_WAIT_TIMEOUT_MS);

    Duration::from_millis(configured.max(MIN_WRITE_GATE_WAIT_TIMEOUT_MS))
}

fn write_gate_closed() -> PersistenceError {
    PersistenceError::Storage("Turso write gate closed".to_string())
}

fn write_gate_timeout_error(
    operation: &'static str,
    priority: WritePriority,
    timeout: Duration,
) -> PersistenceError {
    PersistenceError::Storage(format!(
        "{operation} ({}) waited more than {}ms for Turso write gate",
        priority.as_str(),
        timeout.as_millis()
    ))
}
