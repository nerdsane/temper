//! Metrics for asynchronous query-plane projection maintenance.

use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Histogram},
};

struct QueryProjectionMetrics {
    update_enqueued_total: Counter<u64>,
    update_error_total: Counter<u64>,
    update_duration_ms: Histogram<f64>,
}

fn metrics() -> &'static QueryProjectionMetrics {
    static METRICS: OnceLock<QueryProjectionMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("temper.runtime");
        QueryProjectionMetrics {
            update_enqueued_total: meter
                .u64_counter("temper_query_projection_update_enqueued_total")
                .with_description(
                    "Durable query-plane projection updates enqueued after entity dispatch.",
                )
                .build(),
            update_error_total: meter
                .u64_counter("temper_query_projection_update_error_total")
                .with_description("Background durable query-plane projection updates that failed.")
                .build(),
            update_duration_ms: meter
                .f64_histogram("temper_query_projection_update_duration_ms")
                .with_unit("ms")
                .with_description(
                    "Wall time for background durable query-plane projection maintenance.",
                )
                .build(),
        }
    })
}

fn projection_attrs(tenant: &str, entity_type: &str, operation: &str) -> [KeyValue; 3] {
    [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("entity_type", entity_type.to_string()),
        KeyValue::new("operation", operation.to_string()),
    ]
}

pub(crate) fn record_update_enqueued(tenant: &str, entity_type: &str, operation: &str) {
    metrics()
        .update_enqueued_total
        .add(1, &projection_attrs(tenant, entity_type, operation));
}

pub(crate) fn record_update_duration(
    tenant: &str,
    entity_type: &str,
    operation: &str,
    result: &str,
    duration: Duration,
) {
    let attrs = [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("entity_type", entity_type.to_string()),
        KeyValue::new("operation", operation.to_string()),
        KeyValue::new("result", result.to_string()),
    ];
    metrics()
        .update_duration_ms
        .record(duration.as_secs_f64() * 1000.0, &attrs);
}

pub(crate) fn record_update_error(tenant: &str, entity_type: &str, operation: &str) {
    metrics()
        .update_error_total
        .add(1, &projection_attrs(tenant, entity_type, operation));
}
