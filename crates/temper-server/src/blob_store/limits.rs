//! Process-wide bounds for production blob I/O.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::Semaphore;

const DEFAULT_BLOB_IO_MAX_CONCURRENCY: usize = 32;

pub(super) const BLOB_IO_QUEUE_TIMEOUT: Duration = Duration::from_secs(30);
pub(super) const BLOB_BUFFERED_OPERATION_TIMEOUT: Duration = Duration::from_secs(5 * 60);

pub(super) fn blob_io_semaphore() -> Arc<Semaphore> {
    static SEMAPHORE: OnceLock<Arc<Semaphore>> = OnceLock::new();
    Arc::clone(SEMAPHORE.get_or_init(|| {
        let limit = std::env::var("TEMPER_BLOB_IO_MAX_CONCURRENCY") // determinism-ok: startup-only tuning knob
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_BLOB_IO_MAX_CONCURRENCY);
        Arc::new(Semaphore::new(limit))
    }))
}
