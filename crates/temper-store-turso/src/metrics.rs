use std::sync::OnceLock;
use std::time::{Duration, Instant};

use opentelemetry::metrics::Histogram;
use opentelemetry::{KeyValue, global};

struct TursoMetrics {
    query_duration: Histogram<f64>,
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
