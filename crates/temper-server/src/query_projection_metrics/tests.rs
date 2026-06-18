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
    super::record_update_applied_sequence("tenant", "Session", "upsert", "background_dispatch", 42);
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
