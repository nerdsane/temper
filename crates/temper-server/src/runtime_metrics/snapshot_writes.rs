use std::time::Duration;

use opentelemetry::KeyValue;

use super::metrics;

/// Record a queued snapshot write that the background worker started.
pub fn record_snapshot_write_started() {
    metrics().snapshot_write_started_total.add(1, &[]);
}

/// Record a queued snapshot write failure.
pub fn record_snapshot_write_error() {
    metrics().snapshot_write_error_total.add(1, &[]);
}

/// Record a snapshot enqueue coalesced by stream before storage access.
pub fn record_snapshot_write_coalesced() {
    metrics().snapshot_write_coalesced_total.add(1, &[]);
}

/// Record a stale snapshot enqueue skipped before storage access.
pub fn record_snapshot_write_stale_skipped() {
    metrics().snapshot_write_stale_skipped_total.add(1, &[]);
}

/// Record a snapshot enqueue rejected by bounded queue capacity.
pub fn record_snapshot_write_dropped() {
    metrics().snapshot_write_dropped_total.add(1, &[]);
}

/// Record pending snapshot queue depth.
pub fn record_snapshot_write_queue_depth(depth: u64) {
    metrics().snapshot_write_queue_depth.record(depth, &[]);
}

/// Record how long a snapshot waited before the worker opened storage.
pub fn record_snapshot_write_queue_wait(elapsed: Duration) {
    metrics()
        .snapshot_write_queue_wait_ms
        .record(elapsed.as_secs_f64() * 1000.0, &[]);
}

/// Record storage duration for a queued snapshot write.
pub fn record_snapshot_write_duration(result: &str, elapsed: Duration) {
    metrics().snapshot_write_duration_ms.record(
        elapsed.as_secs_f64() * 1000.0,
        &[KeyValue::new("result", result.to_string())],
    );
}

/// Record end-to-end lag from enqueue to snapshot write completion.
pub fn record_snapshot_write_end_to_end_duration(result: &str, elapsed: Duration) {
    metrics().snapshot_write_end_to_end_duration_ms.record(
        elapsed.as_secs_f64() * 1000.0,
        &[KeyValue::new("result", result.to_string())],
    );
}

/// Record the latest snapshot sequence written by the queue.
pub fn record_snapshot_write_applied_sequence(sequence_nr: u64) {
    metrics()
        .snapshot_write_applied_sequence
        .record(sequence_nr, &[]);
}
