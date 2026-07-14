//! Integration test: idle passivation and lazy respawn.

mod common;

use temper_runtime::persistence::{
    COMPOSITE_EVENT_TYPE, EventMetadata, EventStore, PersistenceEnvelope,
};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_store_sim::{SimEventStore, SimFaultConfig};

const TIMED_TASK_IOA: &str = r#"
[automaton]
name = "TimedTask"
states = ["Idle", "Running", "TimedOut"]
initial = "Idle"
allow_indefinite_states = ["Idle", "TimedOut"]

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Running"

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
async fn passivated_actor_respawns_with_correct_state() {
    let seed = 42;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let state = common::build_default_state_with_store(sim_store.clone(), "passivation-test");

    let tenant = TenantId::default();
    let entity_id = format!("o-passive-{seed}");

    let r = common::dispatch(
        &state,
        &tenant,
        "Order",
        &entity_id,
        "AddItem",
        serde_json::json!({}),
    )
    .await
    .expect("AddItem should succeed");
    assert!(r.success);

    let r = common::dispatch(
        &state,
        &tenant,
        "Order",
        &entity_id,
        "SubmitOrder",
        serde_json::json!({}),
    )
    .await
    .expect("SubmitOrder should succeed");
    assert!(r.success);
    assert_eq!(r.state.status, "Submitted");

    let actor_key = format!("{tenant}:Order:{entity_id}");
    assert!(
        state
            .actor_registry
            .read()
            .unwrap()
            .contains_key(&actor_key)
    );

    // Force this actor to appear idle beyond the default timeout (300s).
    {
        let mut last_accessed = state.last_accessed.write().unwrap();
        last_accessed.insert(
            actor_key.clone(),
            sim_now() - chrono::Duration::seconds(600),
        );
    }

    state.passivate_idle_actors().await;

    assert!(
        !state
            .actor_registry
            .read()
            .unwrap()
            .contains_key(&actor_key),
        "actor should be removed from registry after passivation"
    );

    let snapshot = sim_store
        .load_snapshot(&actor_key)
        .await
        .expect("snapshot lookup should succeed");
    assert!(snapshot.is_some(), "passivation should persist a snapshot");

    let recovered = state
        .get_tenant_entity_state(&tenant, "Order", &entity_id)
        .await
        .expect("lazy respawn should rebuild actor state");

    assert_eq!(recovered.state.status, "Submitted");
    assert_eq!(recovered.state.item_count, 1);
    assert!(recovered.state.total_event_count >= 3); // Created + AddItem + SubmitOrder
}

#[tokio::test]
async fn passivation_snapshot_preserves_state_timeout_clock_anchor() {
    let seed = 203;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "passivation-timeout-anchor",
        "default",
        &[("TimedTask", TIMED_TASK_IOA)],
    );
    let tenant = TenantId::default();
    let entity_id = "timed-passivation";
    let actor_key = format!("{tenant}:TimedTask:{entity_id}");

    let started = common::dispatch(
        &state,
        &tenant,
        "TimedTask",
        entity_id,
        "Start",
        serde_json::json!({}),
    )
    .await
    .expect("Start should enter the timed state");
    let reset_at = started
        .state
        .state_timeout_clock_reset_at
        .expect("live transition records the durable timeout anchor");

    state.last_accessed.write().unwrap().insert(
        actor_key.clone(),
        sim_now() - chrono::Duration::seconds(600),
    );
    state.passivate_idle_actors().await;

    let (_, snapshot_bytes) = sim_store
        .load_snapshot(&actor_key)
        .await
        .expect("snapshot lookup succeeds")
        .expect("passivation writes a snapshot");
    let snapshot: serde_json::Value =
        serde_json::from_slice(&snapshot_bytes).expect("passivation snapshot is JSON");
    assert_eq!(
        snapshot.get("state_timeout_clock_reset_at"),
        Some(&serde_json::json!(reset_at)),
        "passivation must use the same timeout-aware snapshot encoder"
    );
    assert!(snapshot.get("events").is_none());

    let recovered = state
        .get_tenant_entity_state(&tenant, "TimedTask", entity_id)
        .await
        .expect("lazy respawn restores the timed actor");
    assert_eq!(
        recovered.state.state_timeout_clock_reset_at,
        Some(reset_at),
        "respawn must restore the exact passivation snapshot anchor"
    );
}

async fn assert_legacy_snapshot_anchor_repair_survives_restart(
    seed: u64,
    entity_id: &str,
    with_composite_tail: bool,
) {
    let (_guard, clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let actor_key = format!("default:TimedTask:{entity_id}");
    let event = |action: &str, from: &str, to: &str| PersistenceEnvelope {
        sequence_nr: 0,
        event_type: action.to_string(),
        payload: serde_json::json!({
            "action": action,
            "from_status": from,
            "to_status": to,
            "timestamp": sim_now(),
            "params": {}
        }),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: sim_now(),
            actor_id: actor_key.clone(),
        },
    };
    sim_store
        .append(
            &actor_key,
            0,
            &[
                event("Created", "", "Idle"),
                event("Start", "Idle", "Running"),
            ],
        )
        .await
        .expect("seed legacy timed history");
    let legacy_snapshot = serde_json::json!({
        "entity_type": "TimedTask",
        "entity_id": entity_id,
        "status": "Running",
        "item_count": 0,
        "counters": {},
        "booleans": {},
        "lists": {},
        "fields": {"Id": entity_id, "Status": "Running"},
        "total_event_count": 0,
        "events_since_snapshot": 0,
        "last_snapshot_sequence_nr": 2,
        "sequence_nr": 2,
        "processed_idempotency_keys": {}
    });
    sim_store
        .save_snapshot(
            &actor_key,
            2,
            &serde_json::to_vec(&legacy_snapshot).expect("legacy snapshot serialization"),
        )
        .await
        .expect("seed legacy snapshot without timeout anchor");
    if with_composite_tail {
        sim_store
            .append(
                &actor_key,
                2,
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
            .expect("seed post-snapshot composite marker");
    }
    let expected_sequence_nr = if with_composite_tail { 3 } else { 2 };
    let expected_replayed_tail = if with_composite_tail { 1 } else { 0 };
    let segments_before_repair = sim_store.dump_segments(&actor_key);

    let tenant = TenantId::default();
    sim_store.restore_faults(SimFaultConfig {
        snapshot_failure_prob: 1.0,
        ..SimFaultConfig::none()
    });
    let failed_state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "legacy-timeout-repair-write-failure",
        "default",
        &[("TimedTask", TIMED_TASK_IOA)],
    );
    assert!(
        failed_state
            .get_tenant_entity_state(&tenant, "TimedTask", entity_id)
            .await
            .is_err(),
        "hydration must fail instead of exposing a refreshable in-memory timeout anchor"
    );
    assert!(
        failed_state
            .state_timeout_tracker
            .pending_snapshot()
            .is_empty(),
        "a failed durable repair must not arm a timeout"
    );
    let (_, still_legacy_snapshot_bytes) = sim_store
        .load_snapshot(&actor_key)
        .await
        .expect("legacy snapshot lookup after injected write failure")
        .expect("legacy snapshot remains present after injected write failure");
    let still_legacy_snapshot: serde_json::Value =
        serde_json::from_slice(&still_legacy_snapshot_bytes).expect("legacy snapshot JSON");
    assert!(
        still_legacy_snapshot
            .get("state_timeout_clock_reset_at")
            .is_none(),
        "failed upgrade must not report an anchor that was never persisted"
    );
    let failed_actor_uid = failed_state
        .actor_registry
        .read()
        .unwrap()
        .get(&actor_key)
        .expect("failed actor remains observable until the next access")
        .id()
        .uid;
    sim_store.disable_faults();

    let expected_repair_at = sim_now();
    let first_recovery = failed_state
        .get_tenant_entity_state(&tenant, "TimedTask", entity_id)
        .await
        .expect("the same server respawns the entity after snapshot storage recovers");
    let recovered_actor_uid = failed_state
        .actor_registry
        .read()
        .unwrap()
        .get(&actor_key)
        .expect("recovered actor is registered")
        .id()
        .uid;
    assert_ne!(
        recovered_actor_uid, failed_actor_uid,
        "recovery must replace the permanently stopped actor incarnation"
    );
    assert_eq!(
        first_recovery.state.state_timeout_clock_reset_at,
        Some(expected_repair_at),
        "legacy hydration establishes one conservative current anchor"
    );
    assert_eq!(
        first_recovery.state.sequence_nr, expected_sequence_nr,
        "legacy hydration must not append another bootstrap Created event"
    );
    assert_eq!(first_recovery.state.last_snapshot_sequence_nr, 2);
    assert_eq!(
        first_recovery.state.events_since_snapshot, expected_replayed_tail,
        "the live replay budget must retain skipped post-snapshot envelopes"
    );
    let journal_after_recovery = sim_store
        .read_events(&actor_key, 0)
        .await
        .expect("read journal after legacy hydration");
    assert_eq!(
        journal_after_recovery.len() as u64,
        expected_sequence_nr,
        "legacy hydration must leave the durable journal unchanged"
    );
    assert_eq!(
        journal_after_recovery.last().map(|event| event.sequence_nr),
        Some(expected_sequence_nr)
    );
    for _ in 0..32 {
        if failed_state.state_timeout_tracker.pending_snapshot()
            == vec![("TimedTask".to_string(), 1)]
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        failed_state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedTask".to_string(), 1)],
        "the repaired legacy state receives one conservative timeout budget"
    );

    let (upgraded_snapshot_sequence, upgraded_snapshot_bytes) = sim_store
        .load_snapshot(&actor_key)
        .await
        .expect("upgraded snapshot lookup succeeds")
        .expect("hydration durably rewrites the legacy snapshot before actor readiness");
    assert_eq!(
        upgraded_snapshot_sequence, 2,
        "the repair must replace the loaded boundary instead of creating a new one"
    );
    let upgraded_snapshot: serde_json::Value =
        serde_json::from_slice(&upgraded_snapshot_bytes).expect("upgraded snapshot JSON");
    assert_eq!(
        upgraded_snapshot.get("state_timeout_clock_reset_at"),
        Some(&serde_json::json!(expected_repair_at))
    );
    assert_eq!(
        upgraded_snapshot.get("sequence_nr"),
        Some(&serde_json::json!(2))
    );
    assert_eq!(
        upgraded_snapshot.get("last_snapshot_sequence_nr"),
        Some(&serde_json::json!(2))
    );
    assert_eq!(
        upgraded_snapshot.get("events_since_snapshot"),
        Some(&serde_json::json!(0))
    );
    assert_eq!(
        sim_store.dump_segments(&actor_key),
        segments_before_repair,
        "legacy metadata repair must not rotate the existing snapshot boundary"
    );

    drop(failed_state);
    clock.advance_by(100);
    let second_state = common::build_single_tenant_state_with_store(
        sim_store,
        "legacy-timeout-repair-second",
        "default",
        &[("TimedTask", TIMED_TASK_IOA)],
    );
    let second_recovery = second_state
        .get_tenant_entity_state(&tenant, "TimedTask", entity_id)
        .await
        .expect("current snapshot hydrates after a second restart");
    assert_eq!(
        second_recovery.state.state_timeout_clock_reset_at,
        Some(expected_repair_at),
        "the second restart must retain the first repair instead of refreshing the budget"
    );
    assert_ne!(
        second_recovery.state.state_timeout_clock_reset_at,
        Some(sim_now()),
        "current snapshots must not be mistaken for legacy snapshots"
    );
    assert_eq!(second_recovery.state.sequence_nr, expected_sequence_nr);
    assert_eq!(second_recovery.state.last_snapshot_sequence_nr, 2);
    assert_eq!(
        second_recovery.state.events_since_snapshot, expected_replayed_tail,
        "the skipped replay tail must remain bounded and replayable after another restart"
    );
}

#[tokio::test]
async fn legacy_snapshot_anchor_repair_survives_immediate_second_restart() {
    assert_legacy_snapshot_anchor_repair_survives_restart(205, "legacy-timed-passivation", false)
        .await;
}

#[tokio::test]
async fn legacy_snapshot_anchor_repair_with_composite_tail_survives_restart() {
    assert_legacy_snapshot_anchor_repair_survives_restart(206, "legacy-timed-composite-tail", true)
        .await;
}

#[tokio::test]
async fn legacy_timeout_anchor_without_snapshot_creates_durable_boundary() {
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

    let expected_repair_at = sim_now();
    let first_state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "legacy-timeout-no-snapshot-first",
        "default",
        &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
    );
    let first_recovery = first_state
        .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
        .await
        .expect("legacy hydration creates its first durable snapshot boundary");
    assert_eq!(first_recovery.state.status, "Running");
    assert_eq!(first_recovery.state.sequence_nr, 1);
    assert_eq!(first_recovery.state.last_snapshot_sequence_nr, 1);
    assert_eq!(first_recovery.state.events_since_snapshot, 0);
    assert_eq!(
        first_recovery.state.state_timeout_clock_reset_at,
        Some(expected_repair_at)
    );
    assert_eq!(
        sim_store
            .read_events(&actor_key, 0)
            .await
            .expect("legacy journal remains readable")
            .len(),
        1,
        "repair must create a snapshot boundary without appending a domain event"
    );
    let (snapshot_sequence, snapshot_bytes) = sim_store
        .load_snapshot(&actor_key)
        .await
        .expect("repaired snapshot lookup succeeds")
        .expect("repair writes the first snapshot boundary");
    assert_eq!(snapshot_sequence, 1);
    let snapshot: serde_json::Value =
        serde_json::from_slice(&snapshot_bytes).expect("repaired snapshot is JSON");
    assert_eq!(
        snapshot.get("state_timeout_clock_reset_at"),
        Some(&serde_json::json!(expected_repair_at))
    );
    assert_eq!(snapshot.get("sequence_nr"), Some(&serde_json::json!(1)));
    assert_eq!(
        snapshot.get("last_snapshot_sequence_nr"),
        Some(&serde_json::json!(1))
    );
    assert_eq!(
        snapshot.get("events_since_snapshot"),
        Some(&serde_json::json!(0))
    );
    for _ in 0..32 {
        if first_state.state_timeout_tracker.pending_snapshot()
            == vec![("InitialTimedTask".to_string(), 1)]
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        first_state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "the repaired initial timed state receives exactly one timeout"
    );

    drop(first_state);
    clock.advance_by(100);
    let second_state = common::build_single_tenant_state_with_store(
        sim_store,
        "legacy-timeout-no-snapshot-second",
        "default",
        &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
    );
    let second_recovery = second_state
        .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
        .await
        .expect("the new boundary remains readable after an immediate restart");
    assert_eq!(
        second_recovery.state.state_timeout_clock_reset_at,
        Some(expected_repair_at),
        "restart must retain the first conservative anchor"
    );
    assert_ne!(
        second_recovery.state.state_timeout_clock_reset_at,
        Some(sim_now()),
        "restart must not refresh the timeout budget"
    );
    assert_eq!(second_recovery.state.sequence_nr, 1);
    assert_eq!(second_recovery.state.last_snapshot_sequence_nr, 1);
    assert_eq!(second_recovery.state.events_since_snapshot, 0);
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
