//! Trying again: building an exporter, and sending a batch that failed.
//!
//! The SDK's batch processors send each batch once and drop it when the send
//! fails. At a handoff, the connections of every app process on the sandbox
//! are reset at the same moment, not only those of the process being
//! replaced (`Connection reset by peer`, at every staging handoff on 30 and
//! 31 October 2026, for one to two seconds). So the spans of a boot's first
//! second, such as opening the store, and the batch any other running
//! process sent then were lost for good. [`Retrying`] sends a failed batch
//! again after each of [`RETRY_DELAYS`], which together outlast that window,
//! and drops it as before only after the last.

use std::future::ready;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use futures::future::BoxFuture;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::error::OTelSdkResult;
use opentelemetry_sdk::logs::{LogBatch, LogExporter};
use opentelemetry_sdk::trace::{SpanData, SpanExporter};

const OTEL_EXPORTER_BUILD_RETRY_ATTEMPTS: usize = 3;
const OTEL_EXPORTER_RETRY_BASE_DELAY_MS: u64 = 250;

pub(super) fn build_with_retry<T, E, F>(
    component: &str,
    mut build: F,
) -> Result<T, Box<dyn std::error::Error>>
where
    E: std::error::Error + 'static,
    F: FnMut() -> Result<T, E>,
{
    let mut last_err: Option<Box<dyn std::error::Error>> = None;

    for attempt in 1..=OTEL_EXPORTER_BUILD_RETRY_ATTEMPTS {
        match build() {
            Ok(exporter) => return Ok(exporter),
            Err(err) => {
                eprintln!(
                    "OTEL {component} init failed (attempt {attempt}/{OTEL_EXPORTER_BUILD_RETRY_ATTEMPTS}): {err}"
                );
                last_err = Some(Box::new(err));
                if attempt < OTEL_EXPORTER_BUILD_RETRY_ATTEMPTS {
                    let backoff_ms = OTEL_EXPORTER_RETRY_BASE_DELAY_MS * attempt as u64;
                    // Blocking sleep is intentional: this runs once at startup
                    // before the async runtime is accepting work.
                    std::thread::sleep(Duration::from_millis(backoff_ms));
                }
            }
        }
    }

    Err(last_err.unwrap_or_else(|| {
        Box::new(std::io::Error::other(format!(
            "OTEL {component} init failed without a concrete error",
        )))
    }))
}

/// How long to wait before each further send of a failed batch: 3.75 s in
/// all. The batch's processor waits meanwhile and keeps new telemetry in its
/// queue, up to the queue's size.
pub(super) const RETRY_DELAYS: [Duration; 4] = [
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
    Duration::from_secs(2),
];

/// Set once the process starts shutting its telemetry down. A send that
/// fails after that is not tried again, so a stopping server's last flush
/// never holds up its exit. That flush is still lost when it lands in a
/// handoff's reset.
#[derive(Clone, Debug, Default)]
pub(super) struct Closing(Arc<(Mutex<bool>, Condvar)>);

impl Closing {
    pub(super) fn close(&self) {
        let (closed, wake) = &*self.0;
        *closed.lock().expect("closing mutex must lock") = true;
        wake.notify_all();
    }

    /// Wait before retrying, waking immediately if shutdown starts. The
    /// predicate handles closing before the wait and spurious wakeups; the
    /// final check prevents a resend when closing interrupted the wait.
    fn wait_before_retry(&self, delay: Duration) -> bool {
        let (closed, wake) = &*self.0;
        let closed = closed.lock().expect("closing mutex must lock");
        let (closed, _) = wake
            .wait_timeout_while(closed, delay, |closed| !*closed)
            .expect("closing mutex must lock after waiting");
        !*closed
    }
}

/// An exporter that sends a failed batch again, after each of its delays,
/// until a send succeeds or the process starts closing.
#[derive(Debug)]
pub(super) struct Retrying<E> {
    inner: E,
    closing: Closing,
    delays: &'static [Duration],
}

impl<E> Retrying<E> {
    pub(super) fn new(inner: E, closing: &Closing) -> Self {
        Self::with_delays(inner, closing, &RETRY_DELAYS)
    }

    pub(super) fn with_delays(inner: E, closing: &Closing, delays: &'static [Duration]) -> Self {
        Self {
            inner,
            closing: closing.clone(),
            delays,
        }
    }

    /// Whether to send again after `result`, waiting `delay` first.
    fn again(&self, result: &OTelSdkResult, delay: Duration) -> bool {
        // The processors export on a thread of their own, never the async
        // runtime's, so waiting here holds up only that thread.
        result.is_err() && self.closing.wait_before_retry(delay)
    }
}

impl<E: SpanExporter> SpanExporter for Retrying<E> {
    fn export(&mut self, batch: Vec<SpanData>) -> BoxFuture<'static, OTelSdkResult> {
        // The processor calls this outside its own `block_on`, so blocking on
        // each send here does not nest executors.
        let mut result = futures::executor::block_on(self.inner.export(batch.clone()));
        for &delay in self.delays {
            if !self.again(&result, delay) {
                break;
            }
            result = futures::executor::block_on(self.inner.export(batch.clone()));
        }
        Box::pin(ready(result))
    }

    fn shutdown(&mut self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn force_flush(&mut self) -> OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}

impl<E: LogExporter> LogExporter for Retrying<E> {
    async fn export(&self, batch: LogBatch<'_>) -> OTelSdkResult {
        // The processor polls this inside its own `block_on`, so each send is
        // awaited rather than blocked on.
        let records: Vec<_> = batch.iter().collect();
        let mut result = self.inner.export(LogBatch::new(&records)).await;
        for &delay in self.delays {
            if !self.again(&result, delay) {
                break;
            }
            result = self.inner.export(LogBatch::new(&records)).await;
        }
        result
    }

    fn shutdown(&mut self) -> OTelSdkResult {
        self.inner.shutdown()
    }

    fn set_resource(&mut self, resource: &Resource) {
        self.inner.set_resource(resource);
    }
}
