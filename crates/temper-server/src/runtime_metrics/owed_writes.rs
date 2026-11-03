//! Owed writes held in memory until the store takes them
//! (`state::dispatch::owed_writes`): how many are held now, and each one
//! still held when the process stops.

use std::sync::OnceLock;

use opentelemetry::metrics::{Counter, Gauge};
use opentelemetry::{KeyValue, global};

struct OwedWriteMetrics {
    held: Gauge<u64>,
    held_at_shutdown_total: Counter<u64>,
}

fn metrics() -> &'static OwedWriteMetrics {
    static METRICS: OnceLock<OwedWriteMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("temper.runtime");
        OwedWriteMetrics {
            held: meter
                .u64_gauge("temper_owed_writes_held")
                .with_description(
                    "Writes committed moves owe that the store has not taken yet, held in \
                     memory and tried again until they land or are refused.",
                )
                .build(),
            held_at_shutdown_total: meter
                .u64_counter("temper_owed_writes_held_at_shutdown_total")
                .with_description(
                    "Owed writes still held when the process stopped. Each is lost, except \
                     a state timer's fire, which its row arms again at boot.",
                )
                .build(),
        }
    })
}

/// Record how many owed writes are held now.
pub(crate) fn record_held(count: usize) {
    metrics().held.record(count as u64, &[]);
}

/// Count one owed write still held as the process stops. The log line names
/// its record and key; the metric keeps to bounded attributes.
pub(crate) fn record_held_at_shutdown(tenant: &str, entity_type: &str, owed_write: &str) {
    metrics().held_at_shutdown_total.add(
        1,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("owed_write", owed_write.to_string()),
        ],
    );
}
