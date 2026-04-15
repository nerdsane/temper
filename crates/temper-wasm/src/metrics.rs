use std::sync::OnceLock;

use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::{KeyValue, global};

struct WasmMetrics {
    invocations_total: Counter<u64>,
    invocation_duration_ms: Histogram<f64>,
    host_http_requests_total: Counter<u64>,
    host_http_duration_ms: Histogram<f64>,
    host_http_request_bytes: Histogram<u64>,
    host_http_response_bytes: Histogram<u64>,
    blob_transport_wait_duration_ms: Histogram<f64>,
    blob_transport_requests_total: Counter<u64>,
}

fn metrics() -> &'static WasmMetrics {
    static METRICS: OnceLock<WasmMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("temper.wasm");
        WasmMetrics {
            invocations_total: meter
                .u64_counter("temper_wasm_invocations_total")
                .with_description("Total number of WASM invocations executed by Temper.")
                .build(),
            invocation_duration_ms: meter
                .f64_histogram("temper_wasm_invocation_duration_ms")
                .with_unit("ms")
                .with_description("End-to-end duration of a single WASM invocation.")
                .build(),
            host_http_requests_total: meter
                .u64_counter("temper_wasm_host_http_requests_total")
                .with_description(
                    "Total outbound host HTTP requests made on behalf of WASM modules.",
                )
                .build(),
            host_http_duration_ms: meter
                .f64_histogram("temper_wasm_host_http_duration_ms")
                .with_unit("ms")
                .with_description("Latency of outbound host HTTP requests for WASM modules.")
                .build(),
            host_http_request_bytes: meter
                .u64_histogram("temper_wasm_host_http_request_bytes")
                .with_unit("By")
                .with_description("Payload size of outbound host HTTP requests from WASM modules.")
                .build(),
            host_http_response_bytes: meter
                .u64_histogram("temper_wasm_host_http_response_bytes")
                .with_unit("By")
                .with_description(
                    "Payload size of outbound host HTTP responses returned to WASM modules.",
                )
                .build(),
            blob_transport_wait_duration_ms: meter
                .f64_histogram("temper_blob_transport_wait_duration_ms")
                .with_unit("ms")
                .with_description(
                    "Time spent waiting for shared blob transport capacity before a remote blob request starts.",
                )
                .build(),
            blob_transport_requests_total: meter
                .u64_counter("temper_blob_transport_requests_total")
                .with_description(
                    "Total number of remote blob transport requests issued by WASM modules.",
                )
                .build(),
        }
    })
}

pub(crate) fn record_wasm_invoke(
    entity_type: &str,
    trigger_action: &str,
    needs_wasi: bool,
    success: bool,
    duration_ms: f64,
) {
    let attrs = [
        KeyValue::new("entity_type", entity_type.to_string()),
        KeyValue::new("trigger_action", trigger_action.to_string()),
        KeyValue::new("needs_wasi", needs_wasi),
        KeyValue::new("success", success),
    ];
    metrics().invocations_total.add(1, &attrs);
    metrics().invocation_duration_ms.record(duration_ms, &attrs);
}

pub(crate) fn record_host_http_call(
    method: &str,
    kind: &str,
    status_code: u16,
    request_bytes: u64,
    response_bytes: u64,
    duration_ms: f64,
) {
    let status_code_class = format!("{}xx", status_code / 100);
    let attrs = [
        KeyValue::new("http_method", method.to_string()),
        KeyValue::new("call_kind", kind.to_string()),
        KeyValue::new("status_code_class", status_code_class),
    ];
    metrics().host_http_requests_total.add(1, &attrs);
    metrics().host_http_duration_ms.record(duration_ms, &attrs);
    metrics()
        .host_http_request_bytes
        .record(request_bytes, &attrs);
    metrics()
        .host_http_response_bytes
        .record(response_bytes, &attrs);
}

pub(crate) fn record_blob_transport_wait(method: &str, backend: &str, wait_duration_ms: f64) {
    let attrs = [
        KeyValue::new("http_method", method.to_string()),
        KeyValue::new("backend", backend.to_string()),
    ];
    metrics()
        .blob_transport_wait_duration_ms
        .record(wait_duration_ms, &attrs);
    metrics().blob_transport_requests_total.add(1, &attrs);
}
