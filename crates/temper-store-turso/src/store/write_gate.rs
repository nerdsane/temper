use std::time::Duration;

use temper_runtime::persistence::PersistenceError;
use tokio::sync::OwnedSemaphorePermit;

use super::TursoEventStore;

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
    /// requests racing for the same remote writer.
    pub(super) async fn acquire_write_permit(
        &self,
        operation: &'static str,
    ) -> Result<OwnedSemaphorePermit, PersistenceError> {
        let started = std::time::Instant::now();
        let timeout = write_gate_wait_timeout();
        let permit = tokio::time::timeout(timeout, self.write_gate.clone().acquire_owned())
            .await
            .map_err(|_| {
                PersistenceError::Storage(format!(
                    "{operation} waited more than {}ms for Turso write gate",
                    timeout.as_millis()
                ))
            })?
            .map_err(|_| PersistenceError::Storage("Turso write gate closed".to_string()))?;

        let waited = started.elapsed();
        if waited >= Duration::from_millis(100) {
            tracing::warn!(
                operation,
                wait_ms = waited.as_millis() as u64,
                "turso.write_gate waited"
            );
        } else {
            tracing::debug!(
                operation,
                wait_ms = waited.as_millis() as u64,
                "turso.write_gate acquired"
            );
        }

        Ok(permit)
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
