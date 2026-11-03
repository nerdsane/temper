use std::sync::OnceLock;
use std::time::{Duration, Instant};

use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::{KeyValue, global};

struct TursoMetrics {
    operation_duration: Histogram<f64>,
    write_retries: Counter<u64>,
    write_gate_wait: Histogram<f64>,
    write_gate_hold: Histogram<f64>,
}

fn metrics() -> &'static TursoMetrics {
    static METRICS: OnceLock<TursoMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("temper.turso");
        TursoMetrics {
            // Its own name: `temper_turso_query_duration` is the per-call
            // histogram in `store::instrumentation`, and two instruments under
            // one name mixed seconds with milliseconds in one series.
            operation_duration: meter
                .f64_histogram("temper_turso_operation_duration")
                .with_unit("ms")
                .with_description(
                    "Duration of one Turso/libSQL store operation, start to finish: its \
                     connection, every query and execute inside it, and reading the rows. \
                     Tagged by operation.",
                )
                .build(),
            write_retries: meter
                .u64_counter("temper_turso_write_retries_total")
                .with_description(
                    "Count of Turso/libSQL write operations that required a retry due to \
                     transient errors (BLOCKED, stream error, timeout). Tagged by operation, \
                     attempt number (1 = first retry), and outcome (succeeded | exhausted).",
                )
                .build(),
            write_gate_wait: meter
                .f64_histogram("temper_turso_write_gate_wait")
                .with_unit("ms")
                .with_description(
                    "How long one write waited for the store's write gate, the one permit \
                     every write to the database takes in turn. Tagged by operation, \
                     priority (high | low) and outcome (acquired | timed_out).",
                )
                .build(),
            write_gate_hold: meter
                .f64_histogram("temper_turso_write_gate_hold")
                .with_unit("ms")
                .with_description(
                    "How long one writer held the store's write gate, from taking its \
                     permit to giving it back: its connection, its transaction and its \
                     commit. Every other write waits that long. Tagged by operation.",
                )
                .build(),
        }
    })
}

pub fn init_metrics() {
    let _ = metrics();
}

pub(crate) fn record_turso_operation_duration(duration: Duration, operation: &str) {
    metrics().operation_duration.record(
        duration.as_secs_f64() * 1_000.0,
        &[KeyValue::new("operation", operation.to_string())],
    );
}

/// Record a Turso write-retry event. `outcome` is `"succeeded"` if the retry
/// eventually produced a successful write, or `"exhausted"` if all attempts
/// failed and the caller received the error.
pub(crate) fn record_turso_write_retry(
    operation: &'static str,
    attempt: u64,
    outcome: &'static str,
) {
    metrics().write_retries.add(
        1,
        &[
            KeyValue::new("operation", operation),
            KeyValue::new("attempt", attempt as i64),
            KeyValue::new("outcome", outcome),
        ],
    );
}

/// Record how long a write waited at the write gate. `outcome` is
/// `"acquired"` when it took the permit, or `"timed_out"` when it gave up.
pub(crate) fn record_turso_write_gate_wait(
    waited: Duration,
    operation: &'static str,
    priority: &'static str,
    outcome: &'static str,
) {
    metrics().write_gate_wait.record(
        waited.as_secs_f64() * 1_000.0,
        &[
            KeyValue::new("operation", operation),
            KeyValue::new("priority", priority),
            KeyValue::new("outcome", outcome),
        ],
    );
}

/// Record how long a writer held the write gate's permit.
pub(crate) fn record_turso_write_gate_hold(held: Duration, operation: &'static str) {
    metrics().write_gate_hold.record(
        held.as_secs_f64() * 1_000.0,
        &[KeyValue::new("operation", operation)],
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
        record_turso_operation_duration(self.started.elapsed(), self.operation);
    }
}
