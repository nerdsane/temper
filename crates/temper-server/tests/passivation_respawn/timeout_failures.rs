//! Fail-closed timeout hydration regressions for ambiguous durable history.

#[path = "timeout_failures/journal_reads.rs"]
mod journal_reads;

use super::common;
use temper_runtime::persistence::{
    COMPOSITE_EVENT_TYPE, EventMetadata, EventStore, PersistenceEnvelope,
};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_store_sim::{SimEventStore, SimFaultConfig};

const INITIAL_TIMED_TASK_IOA: &str = r#"
[automaton]
name = "InitialTimedTask"
states = ["Running", "TimedOut"]
initial = "Running"
allow_indefinite_states = ["TimedOut"]

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"

[[state_timeout]]
state = "Running"
after_seconds = 60
on_timeout = "TimeoutFail"
"#;

#[tokio::test]
async fn legacy_timeout_anchor_without_snapshot_fails_without_hiding_journal() {
    let seed = 207;
    let (_guard, clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "legacy-initial-timed-no-snapshot";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    sim_store
        .append(
            &actor_key,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: COMPOSITE_EVENT_TYPE.to_string(),
                payload: serde_json::json!({}),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: actor_key.clone(),
                },
            }],
        )
        .await
        .expect("seed legacy composite marker without a snapshot boundary");

    let first_state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "legacy-timeout-no-snapshot-first",
        "default",
        &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
    );
    assert!(
        first_state
            .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
            .await
            .is_err(),
        "journal-only timeout hydration must fail rather than invent a snapshot boundary"
    );
    assert!(
        first_state
            .state_timeout_tracker
            .pending_snapshot()
            .is_empty(),
        "failed hydration must not arm a refreshable in-memory timeout"
    );
    assert_eq!(
        sim_store
            .read_events(&actor_key, 0)
            .await
            .expect("legacy journal remains readable")
            .len(),
        1,
        "failed hydration must leave the journal unchanged"
    );
    assert_eq!(
        sim_store
            .load_snapshot(&actor_key)
            .await
            .expect("snapshot absence remains readable"),
        None,
        "failed hydration must not hide replayable history behind a new boundary"
    );

    drop(first_state);
    clock.advance_by(100);
    let second_state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "legacy-timeout-no-snapshot-second",
        "default",
        &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
    );
    assert!(
        second_state
            .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
            .await
            .is_err(),
        "a later restart must fail the same way instead of manufacturing a fresh budget"
    );
    assert!(
        second_state
            .state_timeout_tracker
            .pending_snapshot()
            .is_empty(),
        "a repeated failed hydration must still arm no timeout"
    );
    assert_eq!(
        sim_store
            .load_snapshot(&actor_key)
            .await
            .expect("snapshot absence remains readable after restart"),
        None
    );
    assert_eq!(
        sim_store
            .read_events(&actor_key, 0)
            .await
            .expect("journal remains replayable after restart")
            .len(),
        1
    );
}

#[tokio::test]
async fn snapshot_read_failure_does_not_replace_an_existing_boundary() {
    let seed = 208;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "legacy-initial-timed-unreadable-snapshot";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    sim_store
        .append(
            &actor_key,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: COMPOSITE_EVENT_TYPE.to_string(),
                payload: serde_json::json!({}),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: actor_key.clone(),
                },
            }],
        )
        .await
        .expect("seed legacy composite marker");
    let legacy_snapshot = serde_json::json!({
        "entity_type": "InitialTimedTask",
        "entity_id": entity_id,
        "status": "Running",
        "item_count": 0,
        "counters": {},
        "booleans": {},
        "lists": {},
        "fields": {
            "Id": entity_id,
            "Status": "Running",
            "LegacyBoundaryMarker": "must-survive"
        },
        "total_event_count": 0,
        "events_since_snapshot": 0,
        "last_snapshot_sequence_nr": 1,
        "sequence_nr": 1,
        "processed_idempotency_keys": {}
    });
    let legacy_snapshot_bytes =
        serde_json::to_vec(&legacy_snapshot).expect("legacy snapshot serialization");
    sim_store
        .save_snapshot(&actor_key, 1, &legacy_snapshot_bytes)
        .await
        .expect("seed existing legacy snapshot boundary");
    let segments_before = sim_store.dump_segments(&actor_key);

    sim_store.restore_faults(SimFaultConfig {
        snapshot_load_failure_prob: 1.0,
        ..SimFaultConfig::none()
    });
    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "legacy-timeout-snapshot-read-failure",
        "default",
        &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
    );
    assert!(
        state
            .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
            .await
            .is_err(),
        "an unreadable snapshot must fail closed instead of being mistaken for an absent boundary"
    );
    assert!(
        state.state_timeout_tracker.pending_snapshot().is_empty(),
        "failed hydration must not arm a timeout"
    );

    sim_store.disable_faults();
    let (snapshot_sequence, snapshot_bytes) = sim_store
        .load_snapshot(&actor_key)
        .await
        .expect("snapshot becomes readable after storage recovers")
        .expect("the existing snapshot boundary remains present");
    assert_eq!(snapshot_sequence, 1);
    assert_eq!(
        snapshot_bytes, legacy_snapshot_bytes,
        "hydration must not overwrite a boundary it failed to read"
    );
    assert_eq!(
        sim_store.dump_segments(&actor_key),
        segments_before,
        "failed hydration must not rotate existing segment metadata"
    );
}

#[tokio::test]
async fn incompatible_journal_without_snapshot_is_not_sealed_behind_repair_boundary() {
    let seed = 209;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "legacy-initial-timed-incompatible-journal";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    sim_store
        .append(
            &actor_key,
            0,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "LegacyUnknownEvent".to_string(),
                payload: serde_json::json!({ "legacy_shape": true }),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: actor_key.clone(),
                },
            }],
        )
        .await
        .expect("seed an incompatible legacy event without a snapshot boundary");

    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "legacy-timeout-incompatible-journal",
        "default",
        &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
    );
    assert!(
        state
            .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
            .await
            .is_err(),
        "hydration must fail rather than snapshot past an event the current runtime cannot replay"
    );
    assert!(
        state.state_timeout_tracker.pending_snapshot().is_empty(),
        "failed hydration must not arm a timeout"
    );
    assert_eq!(
        sim_store
            .load_snapshot(&actor_key)
            .await
            .expect("snapshot absence remains readable"),
        None,
        "an incompatible event must remain visible to a future compatible runtime"
    );
    assert_eq!(
        sim_store
            .read_events(&actor_key, 0)
            .await
            .expect("legacy journal remains readable")
            .len(),
        1
    );
}
