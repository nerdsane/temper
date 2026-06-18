use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Gauge, Histogram},
};

use super::OtsTrajectoryWrite;

struct OtsOutboxMetrics {
    depth: Gauge<u64>,
    capacity: Gauge<u64>,
    enqueued_total: Counter<u64>,
    rejected_total: Counter<u64>,
    retry_total: Counter<u64>,
    persisted_total: Counter<u64>,
    failed_total: Counter<u64>,
    persist_latency_ms: Histogram<f64>,
}

fn metrics() -> &'static OtsOutboxMetrics {
    static METRICS: OnceLock<OtsOutboxMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("temper.runtime");
        OtsOutboxMetrics {
            depth: meter
                .u64_gauge("temper_ots_trajectory_outbox_depth")
                .with_description("Current number of OTS trajectory artifacts pending persistence.")
                .build(),
            capacity: meter
                .u64_gauge("temper_ots_trajectory_outbox_capacity")
                .with_description("Configured OTS trajectory persistence outbox capacity.")
                .build(),
            enqueued_total: meter
                .u64_counter("temper_ots_trajectory_outbox_enqueued_total")
                .with_description("OTS trajectory artifacts accepted by the persistence outbox.")
                .build(),
            rejected_total: meter
                .u64_counter("temper_ots_trajectory_outbox_rejected_total")
                .with_description("OTS trajectory artifacts rejected because the outbox was full.")
                .build(),
            retry_total: meter
                .u64_counter("temper_ots_trajectory_outbox_retry_total")
                .with_description("OTS trajectory persistence attempts scheduled for retry.")
                .build(),
            persisted_total: meter
                .u64_counter("temper_ots_trajectory_outbox_persisted_total")
                .with_description("OTS trajectory artifacts persisted by the outbox.")
                .build(),
            failed_total: meter
                .u64_counter("temper_ots_trajectory_outbox_failed_total")
                .with_description("OTS trajectory artifacts that exhausted outbox retries.")
                .build(),
            persist_latency_ms: meter
                .f64_histogram("temper_ots_trajectory_outbox_persist_latency_ms")
                .with_unit("ms")
                .with_description("Wall time for one OTS trajectory persistence attempt.")
                .build(),
        }
    })
}

fn attrs(item: &OtsTrajectoryWrite, backend: &'static str) -> [KeyValue; 3] {
    [
        KeyValue::new("tenant", item.tenant.clone()),
        KeyValue::new("outcome", item.outcome.clone()),
        KeyValue::new("backend", backend.to_string()),
    ]
}

pub(super) fn record_depth(depth: usize) {
    metrics().depth.record(depth as u64, &[]);
}

pub(super) fn record_capacity(capacity: usize) {
    metrics().capacity.record(capacity as u64, &[]);
}

pub(super) fn record_enqueue(item: &OtsTrajectoryWrite, backend: &'static str) {
    metrics().enqueued_total.add(1, &attrs(item, backend));
}

pub(super) fn record_rejected(item: &OtsTrajectoryWrite, backend: &'static str) {
    metrics().rejected_total.add(1, &attrs(item, backend));
}

pub(super) fn record_retry(item: &OtsTrajectoryWrite, backend: &'static str) {
    metrics().retry_total.add(1, &attrs(item, backend));
}

pub(super) fn record_persisted(item: &OtsTrajectoryWrite, backend: &'static str) {
    metrics().persisted_total.add(1, &attrs(item, backend));
}

pub(super) fn record_failed(item: &OtsTrajectoryWrite, backend: &'static str) {
    metrics().failed_total.add(1, &attrs(item, backend));
}

pub(super) fn record_persist_latency(
    item: &OtsTrajectoryWrite,
    backend: &'static str,
    result: &str,
    duration: Duration,
) {
    let attrs = [
        KeyValue::new("tenant", item.tenant.clone()),
        KeyValue::new("outcome", item.outcome.clone()),
        KeyValue::new("backend", backend.to_string()),
        KeyValue::new("result", result.to_string()),
    ];
    metrics()
        .persist_latency_ms
        .record(duration.as_secs_f64() * 1000.0, &attrs);
}
