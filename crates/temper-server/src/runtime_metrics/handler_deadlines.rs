use std::time::Duration;

use opentelemetry::KeyValue;

use super::metrics;

/// Record the deadline headroom (budget remaining) at WASM dispatch start.
pub fn record_handler_deadline_remaining(entity_type: &str, action: &str, remaining: Duration) {
    metrics().handler_deadline_remaining_ms.record(
        (remaining.as_secs_f64() * 1000.0).max(0.0) as u64,
        &[
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("action", action.to_string()),
        ],
    );
}

/// Record a handler killed for exceeding its deadline.
///
/// `dying_span` identifies which host function was running when the guest
/// was terminated, such as `wasm.web_search` or `wasm.provider_call`.
pub fn record_handler_deadline_exceeded(entity_type: &str, action: &str, dying_span: &'static str) {
    metrics().handler_deadline_exceeded_total.add(
        1,
        &[
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("action", action.to_string()),
            KeyValue::new("dying_span", dying_span),
        ],
    );
}

/// Record the observed interval between Wasmtime epoch ticks.
pub fn record_wasm_epoch_tick_interval(elapsed: Duration) {
    metrics()
        .wasm_epoch_tick_interval_ms
        .record(elapsed.as_secs_f64() * 1000.0, &[]);
}

/// Record the time from deadline breach to guest exit completion.
pub fn record_handler_kill_latency(entity_type: &str, elapsed: Duration) {
    metrics().handler_kill_latency_ms.record(
        elapsed.as_secs_f64() * 1000.0,
        &[KeyValue::new("entity_type", entity_type.to_string())],
    );
}
