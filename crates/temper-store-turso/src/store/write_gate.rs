use std::time::Duration;

use temper_runtime::persistence::PersistenceError;
use tokio::sync::OwnedSemaphorePermit;

use super::TursoEventStore;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WritePriority {
    High,
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

pub(super) fn configured_write_concurrency(is_remote: bool) -> usize {
    let default = if is_remote { 1 } else { 4 };
    std::env::var("TEMPER_TURSO_WRITE_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(default)
        .max(1)
}

impl TursoEventStore {
    /// Acquire the process-local Turso write lane.
    ///
    /// Remote Turso serializes writes at the database. Letting every actor open
    /// an immediate transaction concurrently turns ordinary write pressure into
    /// a lock/timeout storm. This gate makes the queue explicit in-process, so
    /// timeout budgets apply to the actual database operation instead of many
    /// requests racing for the same remote writer. Atomic foreground writes can
    /// deliberately bypass this lane; they still mark themselves as high
    /// priority so new low-priority writes yield while the foreground write is
    /// in flight.
    pub(super) async fn acquire_write_permit(
        &self,
        operation: &'static str,
        priority: WritePriority,
    ) -> Result<OwnedSemaphorePermit, PersistenceError> {
        let started = std::time::Instant::now();
        let timeout = write_gate_wait_timeout();

        if priority == WritePriority::Low {
            self.wait_for_high_priority_writers(operation, started, timeout)
                .await?;
        }

        let remaining = timeout.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(write_gate_timeout_error(operation, priority, timeout));
        }

        let high_priority_waiter = if priority == WritePriority::High {
            Some(HighPriorityWriteMarker::new(self))
        } else {
            None
        };
        let permit = tokio::time::timeout(remaining, self.write_gate.clone().acquire_owned())
            .await
            .map_err(|_| write_gate_timeout_error(operation, priority, timeout))?
            .map_err(|_| PersistenceError::Storage("Turso write gate closed".to_string()))?;
        if let Some(waiter) = high_priority_waiter {
            waiter.done_waiting();
        }

        let waited = started.elapsed();
        if waited >= Duration::from_millis(100) {
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

    pub(super) fn mark_high_priority_write(
        &self,
        operation: &'static str,
    ) -> HighPriorityWriteMarker<'_> {
        tracing::debug!(
            operation,
            priority = WritePriority::High.as_str(),
            "turso.write_gate bypassed for atomic high-priority write"
        );
        HighPriorityWriteMarker::new(self)
    }

    async fn wait_for_high_priority_writers(
        &self,
        operation: &'static str,
        started: std::time::Instant,
        timeout: Duration,
    ) -> Result<(), PersistenceError> {
        while self.has_high_priority_waiters() {
            let remaining = timeout.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(write_gate_timeout_error(
                    operation,
                    WritePriority::Low,
                    timeout,
                ));
            }
            tokio::time::sleep(remaining.min(Duration::from_millis(25))).await;
        }
        Ok(())
    }

    fn has_high_priority_waiters(&self) -> bool {
        self.high_priority_write_waiters
            .load(std::sync::atomic::Ordering::Acquire)
            > 0
    }
}

pub(super) struct HighPriorityWriteMarker<'a> {
    store: &'a TursoEventStore,
}

impl<'a> HighPriorityWriteMarker<'a> {
    fn new(store: &'a TursoEventStore) -> Self {
        store
            .high_priority_write_waiters
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self { store }
    }

    fn done_waiting(self) {
        drop(self);
    }
}

impl Drop for HighPriorityWriteMarker<'_> {
    fn drop(&mut self) {
        self.store
            .high_priority_write_waiters
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
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
