use std::sync::OnceLock;
use std::time::{Duration, Instant};

use opentelemetry::metrics::{Counter, Histogram};
use opentelemetry::{KeyValue, global};

struct PostgresMetrics {
    pool_acquire_duration_ms: Histogram<f64>,
    transaction_begin_duration_ms: Histogram<f64>,
    transaction_commit_duration_ms: Histogram<f64>,
    transaction_duration_ms: Histogram<f64>,
    operation_outcomes_total: Counter<u64>,
    projection_index_fields: Histogram<u64>,
    projection_index_reconciliations_total: Counter<u64>,
    projection_skipped_index_fields_total: Counter<u64>,
}

fn metrics() -> &'static PostgresMetrics {
    static METRICS: OnceLock<PostgresMetrics> = OnceLock::new();
    METRICS.get_or_init(|| {
        let meter = global::meter("temper.postgres");
        PostgresMetrics {
            pool_acquire_duration_ms: meter
                .f64_histogram("temper_postgres_pool_acquire_duration_ms")
                .with_unit("ms")
                .with_description(
                    "Time spent acquiring a PostgreSQL connection from the sqlx pool.",
                )
                .build(),
            transaction_begin_duration_ms: meter
                .f64_histogram("temper_postgres_transaction_begin_duration_ms")
                .with_unit("ms")
                .with_description("Time spent issuing BEGIN after a PostgreSQL connection is acquired.")
                .build(),
            transaction_commit_duration_ms: meter
                .f64_histogram("temper_postgres_transaction_commit_duration_ms")
                .with_unit("ms")
                .with_description("Time spent committing PostgreSQL transactions.")
                .build(),
            transaction_duration_ms: meter
                .f64_histogram("temper_postgres_transaction_duration_ms")
                .with_unit("ms")
                .with_description(
                    "End-to-end PostgreSQL transaction duration including pool wait, BEGIN, SQL work, and COMMIT.",
                )
                .build(),
            operation_outcomes_total: meter
                .u64_counter("temper_postgres_operation_outcomes_total")
                .with_description("PostgreSQL transaction operation outcomes by low-cardinality operation name.")
                .build(),
            projection_index_fields: meter
                .u64_histogram("temper_postgres_projection_index_fields")
                .with_description(
                    "Number of scalar fields indexed into entity_field_index per projection upsert.",
                )
                .build(),
            projection_index_reconciliations_total: meter
                .u64_counter("temper_postgres_projection_index_reconciliations_total")
                .with_description(
                    "Projection field-index reconciliation decisions by low-cardinality path.",
                )
                .build(),
            projection_skipped_index_fields_total: meter
                .u64_counter("temper_postgres_projection_skipped_index_fields_total")
                .with_description(
                    "Projection scalar fields skipped because they exceed PostgreSQL's btree key budget.",
                )
                .build(),
        }
    })
}

/// Initialize PostgreSQL store metrics at process startup.
pub fn init_metrics() {
    let _ = metrics();
}

pub(crate) fn record_postgres_pool_acquire_duration(
    duration: Duration,
    operation: &'static str,
    outcome: &'static str,
) {
    metrics().pool_acquire_duration_ms.record(
        duration.as_secs_f64() * 1000.0,
        &[
            KeyValue::new("operation", operation),
            KeyValue::new("outcome", outcome),
        ],
    );
}

pub(crate) fn record_postgres_transaction_begin_duration(
    duration: Duration,
    operation: &'static str,
    outcome: &'static str,
) {
    metrics().transaction_begin_duration_ms.record(
        duration.as_secs_f64() * 1000.0,
        &[
            KeyValue::new("operation", operation),
            KeyValue::new("outcome", outcome),
        ],
    );
}

pub(crate) fn record_postgres_transaction_commit_duration(
    duration: Duration,
    operation: &'static str,
    outcome: &'static str,
) {
    metrics().transaction_commit_duration_ms.record(
        duration.as_secs_f64() * 1000.0,
        &[
            KeyValue::new("operation", operation),
            KeyValue::new("outcome", outcome),
        ],
    );
}

pub(crate) fn record_postgres_projection_index_fields(indexed_fields: u64, skipped_fields: u64) {
    metrics().projection_index_fields.record(
        indexed_fields,
        &[KeyValue::new("operation", "query_projection_upsert")],
    );
    if skipped_fields > 0 {
        metrics().projection_skipped_index_fields_total.add(
            skipped_fields,
            &[KeyValue::new("operation", "query_projection_upsert")],
        );
    }
}

pub(crate) fn record_postgres_projection_index_reconciliation(path: &'static str) {
    metrics().projection_index_reconciliations_total.add(
        1,
        &[
            KeyValue::new("operation", "query_projection_upsert"),
            KeyValue::new("path", path),
        ],
    );
}

pub(crate) struct PostgresTransactionTimer {
    operation: &'static str,
    started: Instant,
    outcome: &'static str,
}

impl PostgresTransactionTimer {
    pub(crate) fn start(operation: &'static str) -> Self {
        Self {
            operation,
            started: Instant::now(),
            outcome: "error",
        }
    }

    pub(crate) fn set_outcome(&mut self, outcome: &'static str) {
        self.outcome = outcome;
    }
}

impl Drop for PostgresTransactionTimer {
    fn drop(&mut self) {
        let attrs = [
            KeyValue::new("operation", self.operation),
            KeyValue::new("outcome", self.outcome),
        ];
        metrics()
            .transaction_duration_ms
            .record(self.started.elapsed().as_secs_f64() * 1000.0, &attrs);
        metrics().operation_outcomes_total.add(1, &attrs);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn postgres_metrics_can_record_all_measurements() {
        init_metrics();
        record_postgres_pool_acquire_duration(Duration::from_millis(2), "event_append", "ok");
        record_postgres_transaction_begin_duration(Duration::from_millis(1), "event_append", "ok");
        record_postgres_transaction_commit_duration(Duration::from_millis(3), "event_append", "ok");
        record_postgres_projection_index_fields(4, 1);
        record_postgres_projection_index_reconciliation("skipped_unchanged");

        let mut timer = PostgresTransactionTimer::start("event_append");
        timer.set_outcome("ok");
    }
}
