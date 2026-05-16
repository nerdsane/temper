use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::{KeyValue, global};

struct BlobTransportMetrics {
    duration_ms: Histogram<f64>,
    requests_total: Counter<u64>,
    request_bytes: Histogram<u64>,
    response_bytes: Histogram<u64>,
}

fn metrics() -> &'static BlobTransportMetrics {
    static METRICS: OnceLock<BlobTransportMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("temper.runtime");
        BlobTransportMetrics {
            duration_ms: meter
                .f64_histogram("temper_blob_native_transport_duration_ms")
                .with_unit("ms")
                .with_description("Duration of native Temper blob backend operations.")
                .build(),
            requests_total: meter
                .u64_counter("temper_blob_native_transport_requests_total")
                .with_description("Total number of native Temper blob backend operations.")
                .build(),
            request_bytes: meter
                .u64_histogram("temper_blob_native_transport_request_bytes")
                .with_unit("By")
                .with_description("Request payload size for native Temper blob backend operations.")
                .build(),
            response_bytes: meter
                .u64_histogram("temper_blob_native_transport_response_bytes")
                .with_unit("By")
                .with_description(
                    "Response payload size for native Temper blob backend operations.",
                )
                .build(),
        }
    })
}

pub(crate) fn record(
    duration: Duration,
    operation: &str,
    backend: &str,
    outcome: &str,
    status_code_class: &str,
    request_bytes: u64,
    response_bytes: u64,
) {
    let attrs = [
        KeyValue::new("operation", operation.to_string()),
        KeyValue::new("backend", backend.to_string()),
        KeyValue::new("outcome", outcome.to_string()),
        KeyValue::new("status_code_class", status_code_class.to_string()),
    ];
    metrics()
        .duration_ms
        .record(duration.as_secs_f64() * 1000.0, &attrs);
    metrics().requests_total.add(1, &attrs);
    metrics().request_bytes.record(request_bytes, &attrs);
    metrics().response_bytes.record(response_bytes, &attrs);
}
