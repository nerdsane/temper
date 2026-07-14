//! Timeout-anchor persistence and legacy snapshot upgrade regressions.

use super::common;
use temper_runtime::actor::SystemSignal;
use temper_runtime::persistence::{
    COMPOSITE_EVENT_TYPE, EventMetadata, EventStore, PersistenceEnvelope,
};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::{SimEventStore, SimFaultConfig};

const INITIAL_UNTIMED_TASK_IOA: &str = r#"
[automaton]
name = "InitialTimedTask"
states = ["Running", "TimedOut"]
initial = "Running"
allow_indefinite_states = ["Running", "TimedOut"]

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"
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
after_seconds = 600
on_timeout = "TimeoutFail"
"#;

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

#[tokio::test(start_paused = true)]
async fn hotswap_before_pre_start_cannot_skip_initial_timeout_hydration() {
    let seed = 211;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "hotswap-before-pre-start";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");
    let state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "hotswap-before-pre-start",
        "default",
        &[("InitialTimedTask", INITIAL_UNTIMED_TASK_IOA)],
    );

    // Actor tasks cannot poll until this synchronous test body yields. Spawn
    // under an untimed table, then replace the same live table before pre_start
    // snapshots it. Startup therefore observes the timed definition even
    // though spawn-time admission originally observed the untimed definition.
    state
        .get_or_spawn_tenant_actor(&tenant, "InitialTimedTask", entity_id)
        .expect("spawn the actor before its first task poll");
    {
        let mut registry = state.registry.write().expect("registry lock");
        let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
        registry.register_tenant(
            "default",
            csdl,
            common::CSDL_XML.to_string(),
            &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
        );
    }

    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 1
            && state.state_timeout_tracker.pending_snapshot()
                == vec![("InitialTimedTask".to_string(), 1)]
        {
            break;
        }
    }
    assert_eq!(
        sim_store.total_events(),
        1,
        "pre_start must commit the initial event under the hot-swapped table"
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "a timeout added before pre_start must be hydrated without entity traffic"
    );

    tokio::time::advance(std::time::Duration::from_secs(599)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        sim_store.total_events(),
        1,
        "the hot-swapped timeout must not fire before its original deadline"
    );

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 2 {
            break;
        }
    }
    let journal = sim_store
        .read_events(&actor_key, 0)
        .await
        .expect("read the hot-swap timeout journal");
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "TimeoutFail"],
        "the pre-start hot-swap must still durably fire without a later read"
    );
}

#[tokio::test(start_paused = true)]
async fn slow_successful_pre_start_still_arms_initial_state_timeout() {
    let seed = 209;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "slow-successful-timeout-start";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");

    // Hold the bootstrap append beyond the complete actor-ask retry budget.
    // The actor remains live and eventually starts successfully, so stopped-
    // incarnation replacement cannot recover a lost one-shot hydration task.
    sim_store.inject_append_delay(&actor_key, std::time::Duration::from_secs(120));
    let mut state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "slow-successful-timeout-start",
        "default",
        &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
    );
    state.action_dispatch_timeout = std::time::Duration::from_millis(1);

    let actor_ref = state
        .get_or_spawn_tenant_actor(&tenant, "InitialTimedTask", entity_id)
        .expect("spawn the delayed timed actor");
    tokio::task::yield_now().await;

    // Step virtual time between task turns so every timeout/backoff in the
    // maximum supported 32-attempt policy is created and consumed while the
    // bootstrap append remains blocked. Each attempt needs at most one 800 ms
    // backoff turn and one 1 ms ask-timeout turn.
    for _ in 0..70 {
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(
        sim_store.total_events(),
        0,
        "pre_start must still be waiting after the complete readiness-ask budget"
    );
    assert!(
        !actor_ref.is_stopped(),
        "a slow successful pre_start keeps its mailbox incarnation live"
    );
    assert!(
        state.state_timeout_tracker.pending_snapshot().is_empty(),
        "no timeout can be armed before the actor has recovered its state"
    );

    tokio::time::advance(std::time::Duration::from_secs(50)).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 1 {
            break;
        }
    }
    assert_eq!(
        sim_store.total_events(),
        1,
        "the delayed bootstrap append must eventually complete successfully"
    );

    // The arm must appear after late readiness without any entity request.
    for _ in 0..64 {
        if state.state_timeout_tracker.pending_snapshot()
            == vec![("InitialTimedTask".to_string(), 1)]
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "late successful actor readiness must still arm exactly one initial-state timeout"
    );

    // The 120-second startup delay is charged against the original 600-second
    // budget, leaving exactly 480 seconds after readiness. Prove the timer is
    // neither early nor late and persists its transition before any read.
    tokio::time::advance(std::time::Duration::from_secs(479)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        sim_store.total_events(),
        1,
        "the recovered timeout must not fire before its durable deadline"
    );

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 2 {
            break;
        }
    }
    let journal = sim_store
        .read_events(&actor_key, 0)
        .await
        .expect("read the no-traffic timeout journal");
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "TimeoutFail"],
        "late readiness must still durably fire the initial-state timeout without request traffic"
    );

    let recovered = state
        .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
        .await
        .expect("the actor remains readable after its recovered timeout fires");
    assert_eq!(recovered.state.status, "TimedOut");
}

#[tokio::test(start_paused = true)]
async fn queued_restarts_cannot_overtake_timeout_hydration_handshake() {
    const QUEUED_RESTART_BUDGET: usize = 320;

    let seed = 210;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let sim_store = SimEventStore::no_faults(seed);
    let tenant = TenantId::default();
    let entity_id = "queued-restarts-timeout-start";
    let actor_key = format!("{tenant}:InitialTimedTask:{entity_id}");

    sim_store.inject_append_delay(&actor_key, std::time::Duration::from_secs(120));
    let mut state = common::build_single_tenant_state_with_store(
        sim_store.clone(),
        "queued-restarts-timeout-start",
        "default",
        &[("InitialTimedTask", INITIAL_TIMED_TASK_IOA)],
    );
    state.action_dispatch_timeout = std::time::Duration::from_millis(1);

    let actor_ref = state
        .get_or_spawn_tenant_actor(&tenant, "InitialTimedTask", entity_id)
        .expect("spawn the delayed timed actor");

    // Queue lifecycle work before either spawned task gets a turn. A readiness
    // signal that merely wakes a later hydration ask lets these restarts run
    // first and consume every ask attempt while the actor remains live.
    for _ in 0..QUEUED_RESTART_BUDGET {
        actor_ref
            .signal(SystemSignal::Restart)
            .expect("the bounded mailbox accepts the restart workload");
    }
    tokio::task::yield_now().await;

    tokio::time::advance(std::time::Duration::from_secs(120)).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 1 {
            break;
        }
    }
    assert_eq!(
        sim_store.total_events(),
        1,
        "the delayed initial-state event must commit before reconciliation"
    );

    // One restart is consumed per virtual-time step. This workload exceeds
    // the maximum 32-attempt ask schedule while staying below the fixed 1,000
    // message mailbox budget.
    for _ in 0..QUEUED_RESTART_BUDGET {
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
    }
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    assert!(
        !actor_ref.is_stopped(),
        "queued restarts must leave the actor incarnation live"
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("InitialTimedTask".to_string(), 1)],
        "startup reconciliation must be ordered ahead of already-queued mailbox work"
    );

    // The original deadline remains t=600: 120 seconds in initial startup and
    // 320 seconds draining restarts leave 160 seconds. Prove the transition is
    // durable before any entity request can provide a fallback reconciliation.
    tokio::time::advance(std::time::Duration::from_secs(159)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        sim_store.total_events(),
        1,
        "queued lifecycle work must not move the original timeout deadline"
    );

    tokio::time::advance(std::time::Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim_store.total_events() == 2 {
            break;
        }
    }
    let journal = sim_store
        .read_events(&actor_key, 0)
        .await
        .expect("read the no-traffic timeout journal");
    assert_eq!(
        journal
            .iter()
            .map(|event| event.event_type.as_str())
            .collect::<Vec<_>>(),
        vec!["Created", "TimeoutFail"],
        "queued restarts must not prevent the durable timeout transition"
    );

    let recovered = state
        .get_tenant_entity_state(&tenant, "InitialTimedTask", entity_id)
        .await
        .expect("the actor remains readable after its durable timeout");
    assert_eq!(recovered.state.status, "TimedOut");
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
