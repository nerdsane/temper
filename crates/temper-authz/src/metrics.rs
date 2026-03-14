use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::{KeyValue, global};

struct CedarMetrics {
    evaluations_total: Counter<u64>,
    evaluation_duration: Histogram<f64>,
}

fn metrics() -> &'static CedarMetrics {
    static METRICS: OnceLock<CedarMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("temper.authz");
        CedarMetrics {
            evaluations_total: meter
                .u64_counter("temper_cedar_evaluations_total")
                .with_description("Total number of Cedar authorization evaluations.")
                .build(),
            evaluation_duration: meter
                .f64_histogram("temper_cedar_evaluation_duration")
                .with_description("Latency of Cedar authorization evaluation.")
                .build(),
        }
    })
}

pub fn init_metrics() {
    let _ = metrics();
}

pub(crate) fn record_cedar_evaluation(duration: Duration, decision: &str) {
    let attrs = [KeyValue::new("decision", decision.to_string())];
    metrics().evaluations_total.add(1, &attrs);
    metrics()
        .evaluation_duration
        .record(duration.as_secs_f64(), &attrs);
}
