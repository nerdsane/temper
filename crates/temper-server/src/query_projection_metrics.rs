//! Metrics for asynchronous query-plane projection maintenance.

use std::sync::OnceLock;
use std::time::Duration;

use opentelemetry::{
    KeyValue, global,
    metrics::{Counter, Gauge, Histogram},
};

struct QueryProjectionMetrics {
    update_enqueued_total: Counter<u64>,
    update_started_total: Counter<u64>,
    update_error_total: Counter<u64>,
    update_queue_wait_ms: Histogram<f64>,
    update_duration_ms: Histogram<f64>,
    update_end_to_end_duration_ms: Histogram<f64>,
    update_applied_sequence_nr: Gauge<u64>,
    backfill_entities_total: Counter<u64>,
    backfill_duration_ms: Histogram<f64>,
    backfill_replay_events: Histogram<u64>,
    shadow_check_total: Counter<u64>,
    shadow_check_duration_ms: Histogram<f64>,
    shadow_sequence_gap: Histogram<u64>,
    replay_parity_check_total: Counter<u64>,
    replay_parity_duration_ms: Histogram<f64>,
    replay_parity_sequence_gap: Histogram<u64>,
    replay_parity_run_total: Counter<u64>,
    replay_parity_run_duration_ms: Histogram<f64>,
    replay_parity_run_checked_entities: Histogram<u64>,
    replay_parity_run_drifted_entities: Histogram<u64>,
    replay_parity_run_missing_entities: Histogram<u64>,
    replay_parity_run_error_entities: Histogram<u64>,
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
            update_started_total: meter
                .u64_counter("temper_query_projection_update_started_total")
                .with_description("Durable query-plane projection updates that started running.")
                .build(),
            update_error_total: meter
                .u64_counter("temper_query_projection_update_error_total")
                .with_description("Durable query-plane projection updates that failed.")
                .build(),
            update_queue_wait_ms: meter
                .f64_histogram("temper_query_projection_update_queue_wait_ms")
                .with_unit("ms")
                .with_description(
                    "Time a background durable query-plane projection update waited before running.",
                )
                .build(),
            update_duration_ms: meter
                .f64_histogram("temper_query_projection_update_duration_ms")
                .with_unit("ms")
                .with_description(
                    "Wall time for durable query-plane projection maintenance after work starts.",
                )
                .build(),
            update_end_to_end_duration_ms: meter
                .f64_histogram("temper_query_projection_update_end_to_end_duration_ms")
                .with_unit("ms")
                .with_description(
                    "End-to-end time from durable query-plane projection enqueue to completion.",
                )
                .build(),
            update_applied_sequence_nr: meter
                .u64_gauge("temper_query_projection_applied_sequence_nr")
                .with_description(
                    "Latest authoritative entity sequence number applied to the durable query projection.",
                )
                .build(),
            backfill_entities_total: meter
                .u64_counter("temper_query_projection_backfill_entities_total")
                .with_description(
                    "Entities considered or repaired by startup/recovery query projection backfill.",
                )
                .build(),
            backfill_duration_ms: meter
                .f64_histogram("temper_query_projection_backfill_duration_ms")
                .with_unit("ms")
                .with_description(
                    "Wall time for startup/recovery query projection backfill phases.",
                )
                .build(),
            backfill_replay_events: meter
                .u64_histogram("temper_query_projection_backfill_replay_events")
                .with_description(
                    "Event counts replayed when query projection backfill cannot use a snapshot.",
                )
                .build(),
            shadow_check_total: meter
                .u64_counter("temper_query_projection_shadow_check_total")
                .with_description(
                    "Sampled catalog-fast-read shadow checks comparing projection rows with authoritative actor state.",
                )
                .build(),
            shadow_check_duration_ms: meter
                .f64_histogram("temper_query_projection_shadow_check_duration_ms")
                .with_unit("ms")
                .with_description(
                    "Wall time spent on sampled catalog-fast-read shadow checks.",
                )
                .build(),
            shadow_sequence_gap: meter
                .u64_histogram("temper_query_projection_shadow_sequence_gap")
                .with_description(
                    "Absolute sequence-number gap found by sampled catalog-fast-read shadow checks.",
                )
                .build(),
            replay_parity_check_total: meter
                .u64_counter("temper_query_projection_replay_parity_check_total")
                .with_description(
                    "Projection parity checks comparing durable catalog rows with state rebuilt from the event journal.",
                )
                .build(),
            replay_parity_duration_ms: meter
                .f64_histogram("temper_query_projection_replay_parity_duration_ms")
                .with_unit("ms")
                .with_description(
                    "Wall time spent comparing one entity projection row with replayed authoritative state.",
                )
                .build(),
            replay_parity_sequence_gap: meter
                .u64_histogram("temper_query_projection_replay_parity_sequence_gap")
                .with_description(
                    "Absolute sequence-number gap found by query-projection replay parity verification.",
                )
                .build(),
            replay_parity_run_total: meter
                .u64_counter("temper_query_projection_replay_parity_run_total")
                .with_description(
                    "Bounded query-projection replay parity verifier runs by scope and result.",
                )
                .build(),
            replay_parity_run_duration_ms: meter
                .f64_histogram("temper_query_projection_replay_parity_run_duration_ms")
                .with_unit("ms")
                .with_description("Wall time spent by one replay parity verifier run.")
                .build(),
            replay_parity_run_checked_entities: meter
                .u64_histogram("temper_query_projection_replay_parity_run_checked_entities")
                .with_description("Entities checked by one replay parity verifier run.")
                .build(),
            replay_parity_run_drifted_entities: meter
                .u64_histogram("temper_query_projection_replay_parity_run_drifted_entities")
                .with_description("Drifted entities found by one replay parity verifier run.")
                .build(),
            replay_parity_run_missing_entities: meter
                .u64_histogram("temper_query_projection_replay_parity_run_missing_entities")
                .with_description("Active entities missing from projection during one parity verifier run.")
                .build(),
            replay_parity_run_error_entities: meter
                .u64_histogram("temper_query_projection_replay_parity_run_error_entities")
                .with_description("Entities the replay parity verifier could not compare in one run.")
                .build(),
        }
    })
}

fn projection_attrs(
    tenant: &str,
    entity_type: &str,
    operation: &str,
    source: &str,
) -> [KeyValue; 4] {
    [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("entity_type", entity_type.to_string()),
        KeyValue::new("operation", operation.to_string()),
        KeyValue::new("source", source.to_string()),
    ]
}

pub(crate) fn record_update_enqueued(
    tenant: &str,
    entity_type: &str,
    operation: &str,
    source: &str,
) {
    metrics()
        .update_enqueued_total
        .add(1, &projection_attrs(tenant, entity_type, operation, source));
}

pub(crate) fn record_update_started(
    tenant: &str,
    entity_type: &str,
    operation: &str,
    source: &str,
) {
    metrics()
        .update_started_total
        .add(1, &projection_attrs(tenant, entity_type, operation, source));
}

pub(crate) fn record_update_queue_wait(
    tenant: &str,
    entity_type: &str,
    operation: &str,
    source: &str,
    duration: Duration,
) {
    metrics().update_queue_wait_ms.record(
        duration.as_secs_f64() * 1000.0,
        &projection_attrs(tenant, entity_type, operation, source),
    );
}

pub(crate) fn record_update_duration(
    tenant: &str,
    entity_type: &str,
    operation: &str,
    source: &str,
    result: &str,
    duration: Duration,
) {
    let attrs = [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("entity_type", entity_type.to_string()),
        KeyValue::new("operation", operation.to_string()),
        KeyValue::new("source", source.to_string()),
        KeyValue::new("result", result.to_string()),
    ];
    metrics()
        .update_duration_ms
        .record(duration.as_secs_f64() * 1000.0, &attrs);
}

pub(crate) fn record_update_end_to_end_duration(
    tenant: &str,
    entity_type: &str,
    operation: &str,
    source: &str,
    result: &str,
    duration: Duration,
) {
    let attrs = [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("entity_type", entity_type.to_string()),
        KeyValue::new("operation", operation.to_string()),
        KeyValue::new("source", source.to_string()),
        KeyValue::new("result", result.to_string()),
    ];
    metrics()
        .update_end_to_end_duration_ms
        .record(duration.as_secs_f64() * 1000.0, &attrs);
}

pub(crate) fn record_update_applied_sequence(
    tenant: &str,
    entity_type: &str,
    operation: &str,
    source: &str,
    sequence_nr: u64,
) {
    metrics().update_applied_sequence_nr.record(
        sequence_nr,
        &projection_attrs(tenant, entity_type, operation, source),
    );
}

pub(crate) fn record_update_error(tenant: &str, entity_type: &str, operation: &str, source: &str) {
    metrics()
        .update_error_total
        .add(1, &projection_attrs(tenant, entity_type, operation, source));
}

pub(crate) fn record_backfill_entities(
    tenant: &str,
    entity_type: &str,
    source: &str,
    result: &str,
    count: u64,
) {
    let attrs = [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("entity_type", entity_type.to_string()),
        KeyValue::new("source", source.to_string()),
        KeyValue::new("result", result.to_string()),
    ];
    metrics().backfill_entities_total.add(count, &attrs);
}

pub(crate) fn record_backfill_duration(tenant: &str, phase: &str, duration: Duration) {
    metrics().backfill_duration_ms.record(
        duration.as_secs_f64() * 1000.0,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("phase", phase.to_string()),
        ],
    );
}

pub(crate) fn record_backfill_replay_events(
    tenant: &str,
    entity_type: &str,
    result: &str,
    count: u64,
) {
    metrics().backfill_replay_events.record(
        count,
        &[
            KeyValue::new("tenant", tenant.to_string()),
            KeyValue::new("entity_type", entity_type.to_string()),
            KeyValue::new("result", result.to_string()),
        ],
    );
}

pub(crate) fn record_shadow_check(
    tenant: &str,
    entity_type: &str,
    result: &str,
    drift_kind: &str,
    sequence_direction: &str,
    sequence_gap: u64,
    duration: Duration,
) {
    let attrs = [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("entity_type", entity_type.to_string()),
        KeyValue::new("result", result.to_string()),
        KeyValue::new("drift_kind", drift_kind.to_string()),
        KeyValue::new("sequence_direction", sequence_direction.to_string()),
    ];
    metrics().shadow_check_total.add(1, &attrs);
    metrics()
        .shadow_check_duration_ms
        .record(duration.as_secs_f64() * 1000.0, &attrs);
    metrics().shadow_sequence_gap.record(sequence_gap, &attrs);
}

pub(crate) fn record_replay_parity_check(
    tenant: &str,
    entity_type: &str,
    result: &str,
    drift_kind: &str,
    sequence_direction: &str,
    sequence_gap: u64,
    duration: Duration,
) {
    let attrs = [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("entity_type", entity_type.to_string()),
        KeyValue::new("result", result.to_string()),
        KeyValue::new("drift_kind", drift_kind.to_string()),
        KeyValue::new("sequence_direction", sequence_direction.to_string()),
    ];
    metrics().replay_parity_check_total.add(1, &attrs);
    metrics()
        .replay_parity_duration_ms
        .record(duration.as_secs_f64() * 1000.0, &attrs);
    metrics()
        .replay_parity_sequence_gap
        .record(sequence_gap, &attrs);
}

#[expect(
    clippy::too_many_arguments,
    reason = "metric emission needs explicit low-cardinality dimensions and counts"
)]
pub(crate) fn record_replay_parity_run(
    tenant: &str,
    entity_type: &str,
    source: &str,
    result: &str,
    checked: u64,
    drifted: u64,
    missing: u64,
    errors: u64,
    duration: Duration,
) {
    let attrs = [
        KeyValue::new("tenant", tenant.to_string()),
        KeyValue::new("entity_type", entity_type.to_string()),
        KeyValue::new("source", source.to_string()),
        KeyValue::new("result", result.to_string()),
    ];
    metrics().replay_parity_run_total.add(1, &attrs);
    metrics()
        .replay_parity_run_duration_ms
        .record(duration.as_secs_f64() * 1000.0, &attrs);
    metrics()
        .replay_parity_run_checked_entities
        .record(checked, &attrs);
    metrics()
        .replay_parity_run_drifted_entities
        .record(drifted, &attrs);
    metrics()
        .replay_parity_run_missing_entities
        .record(missing, &attrs);
    metrics()
        .replay_parity_run_error_entities
        .record(errors, &attrs);
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    #[test]
    fn projection_metrics_record_all_observability_slices() {
        super::record_update_enqueued("tenant", "Session", "upsert", "background_dispatch");
        super::record_update_started("tenant", "Session", "upsert", "background_dispatch");
        super::record_update_queue_wait(
            "tenant",
            "Session",
            "upsert",
            "background_dispatch",
            Duration::from_millis(2),
        );
        super::record_update_duration(
            "tenant",
            "Session",
            "upsert",
            "background_dispatch",
            "ok",
            Duration::from_millis(3),
        );
        super::record_update_end_to_end_duration(
            "tenant",
            "Session",
            "upsert",
            "background_dispatch",
            "ok",
            Duration::from_millis(5),
        );
        super::record_update_applied_sequence(
            "tenant",
            "Session",
            "upsert",
            "background_dispatch",
            42,
        );
        super::record_update_error("tenant", "Session", "upsert", "background_dispatch");
        super::record_backfill_entities("tenant", "Session", "backfill_snapshot", "ok", 3);
        super::record_backfill_duration("tenant", "overall", Duration::from_millis(8));
        super::record_backfill_replay_events("tenant", "Session", "ok", 12);
        super::record_shadow_check(
            "tenant",
            "Session",
            "drift",
            "fields",
            "catalog_behind",
            2,
            Duration::from_millis(13),
        );
        super::record_replay_parity_check(
            "tenant",
            "Session",
            "drift",
            "sequence",
            "catalog_behind",
            2,
            Duration::from_millis(21),
        );
        super::record_replay_parity_run(
            "tenant",
            "Session",
            "observe_probe",
            "drift",
            10,
            1,
            0,
            0,
            Duration::from_millis(34),
        );
    }
}
