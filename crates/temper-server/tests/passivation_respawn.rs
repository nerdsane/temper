//! Integration test: idle passivation and lazy respawn.

mod common;

use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_store_sim::SimEventStore;

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

#[tokio::test]
async fn legacy_snapshot_anchor_repair_survives_passivation_and_second_restart() {
    let seed = 205;
    let (_guard, clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let entity_id = "legacy-timed-passivation";
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

    let tenant = TenantId::default();
    let first_state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "legacy-timeout-repair-first",
        "default",
        &[("TimedTask", TIMED_TASK_IOA)],
    );
    let expected_repair_at = sim_now();
    let first_recovery = first_state
        .get_tenant_entity_state(&tenant, "TimedTask", entity_id)
        .await
        .expect("legacy snapshot hydrates");
    assert_eq!(
        first_recovery.state.state_timeout_clock_reset_at,
        Some(expected_repair_at),
        "legacy hydration establishes one conservative current anchor"
    );
    assert_eq!(
        first_recovery.state.sequence_nr, 2,
        "legacy hydration must not append another bootstrap Created event"
    );
    let journal_after_recovery = sim_store
        .read_events(&actor_key, 0)
        .await
        .expect("read journal after legacy hydration");
    assert_eq!(
        journal_after_recovery.len(),
        2,
        "legacy hydration must leave the durable journal unchanged"
    );
    assert_eq!(
        journal_after_recovery.last().map(|event| event.sequence_nr),
        Some(2)
    );
    for _ in 0..32 {
        if first_state.state_timeout_tracker.pending_snapshot()
            == vec![("TimedTask".to_string(), 1)]
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        first_state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedTask".to_string(), 1)],
        "the repaired legacy state receives one conservative timeout budget"
    );

    first_state.last_accessed.write().unwrap().insert(
        actor_key.clone(),
        sim_now() - chrono::Duration::seconds(600),
    );
    first_state.passivate_idle_actors().await;
    let (_, upgraded_snapshot_bytes) = sim_store
        .load_snapshot(&actor_key)
        .await
        .expect("upgraded snapshot lookup succeeds")
        .expect("passivation rewrites the legacy snapshot");
    let upgraded_snapshot: serde_json::Value =
        serde_json::from_slice(&upgraded_snapshot_bytes).expect("upgraded snapshot JSON");
    assert_eq!(
        upgraded_snapshot.get("state_timeout_clock_reset_at"),
        Some(&serde_json::json!(expected_repair_at))
    );

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
}
