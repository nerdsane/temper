use std::sync::OnceLock;
use std::time::{Duration, Instant};

use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::{KeyValue, global};

struct TursoMetrics {
    query_duration: Histogram<f64>,
    write_retries: Counter<u64>,
}

fn metrics() -> &'static TursoMetrics {
    static METRICS: OnceLock<TursoMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("temper.turso");
        TursoMetrics {
            query_duration: meter
                .f64_histogram("temper_turso_query_duration")
                .with_description("Latency of Turso/libSQL query and execute operations.")
                .build(),
            write_retries: meter
                .u64_counter("temper_turso_write_retries_total")
                .with_description(
                    "Count of Turso/libSQL write operations that required a retry due to \
                     transient errors (BLOCKED, stream error, timeout). Tagged by operation, \
                     attempt number (1 = first retry), and outcome (succeeded | exhausted).",
                )
                .build(),
        }
    })
}

pub fn init_metrics() {
    let _ = metrics();
}

pub(crate) fn record_turso_query_duration(duration: Duration, operation: &str) {
    metrics().query_duration.record(
        duration.as_secs_f64(),
        &[KeyValue::new("operation", operation.to_string())],
    );
}

/// Record a Turso write-retry event. `outcome` is `"succeeded"` if the retry
/// eventually produced a successful write, or `"exhausted"` if all attempts
/// failed and the caller received the error.
pub(crate) fn record_turso_write_retry(operation: &'static str, attempt: u64, outcome: &'static str) {
    metrics().write_retries.add(
        1,
        &[
            KeyValue::new("operation", operation),
            KeyValue::new("attempt", attempt as i64),
            KeyValue::new("outcome", outcome),
        ],
    );
}

pub(crate) struct TursoQueryTimer {
    operation: &'static str,
    started: Instant,
}

impl TursoQueryTimer {
    pub(crate) fn start(operation: &'static str) -> Self {
        Self {
            operation,
            started: Instant::now(),
        }
    }
}

impl Drop for TursoQueryTimer {
    fn drop(&mut self) {
        record_turso_query_duration(self.started.elapsed(), self.operation);
    }
}
