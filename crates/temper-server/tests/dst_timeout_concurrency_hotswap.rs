//! DST coverage for timeout hot-swaps inside optimistic-concurrency replay.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use temper_jit::table::TransitionTable;
use temper_runtime::ActorSystem;
use temper_runtime::actor::ActorRef;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_server::entity_actor::StateTimeoutPrecondition;
use temper_server::storage::{BackendLabel, BoxedEventStore};
use temper_server::{EntityActor, EntityMsg, EntityResponse};
use temper_spec::automaton::StateTimeout;
use temper_store_sim::SimEventStore;

const TIMED_IOA: &str = r#"
[automaton]
name = "TimedTask"
states = ["Running", "TimedOut"]
initial = "Running"
allow_indefinite_states = ["TimedOut"]

[[state]]
name = "heartbeats"
type = "counter"
initial = "0"

[[action]]
name = "Heartbeat"
kind = "input"
from = ["Running"]
to = "Running"

[[action]]
name = "Observe"
kind = "input"
from = ["Running"]
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
reset_on = ["Heartbeat"]
"#;

const TIMED_WITHOUT_TIMEOUT_IOA: &str = r#"
[automaton]
name = "TimedTask"
states = ["Running", "TimedOut"]
initial = "Running"
allow_indefinite_states = ["Running", "TimedOut"]

[[state]]
name = "heartbeats"
type = "counter"
initial = "0"

[[action]]
name = "Heartbeat"
kind = "input"
from = ["Running"]
to = "Running"

[[action]]
name = "Observe"
kind = "input"
from = ["Running"]
to = "Running"

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"
"#;

const TIMED_WITH_CHANGED_REPLAY_EFFECT_IOA: &str = r#"
[automaton]
name = "TimedTask"
states = ["Running", "TimedOut"]
initial = "Running"
allow_indefinite_states = ["TimedOut"]

[[state]]
name = "heartbeats"
type = "counter"
initial = "0"

[[action]]
name = "Heartbeat"
kind = "input"
from = ["Running"]
to = "Running"

[[action]]
name = "Observe"
kind = "input"
from = ["Running"]
to = "Running"
effect = [{ type = "increment", var = "heartbeats" }]

[[action]]
name = "TimeoutFail"
kind = "internal"
from = ["Running"]
to = "TimedOut"

[[state_timeout]]
state = "Running"
after_seconds = 60
on_timeout = "TimeoutFail"
reset_on = ["Heartbeat"]
"#;

struct TimeoutHarness {
    actor_ref: ActorRef<EntityMsg>,
    table: Arc<RwLock<TransitionTable>>,
    sim: SimEventStore,
    persistence_id: String,
    expected_timeout: StateTimeout,
    reset_at: chrono::DateTime<chrono::Utc>,
    reset_version: u64,
}

async fn setup_timeout_actor(seed: u64, entity_id: &str) -> TimeoutHarness {
    let sim = SimEventStore::no_faults(seed);
    let table = Arc::new(RwLock::new(TransitionTable::from_ioa_source(TIMED_IOA)));
    let expected_timeout = table
        .read()
        .expect("table lock")
        .state_timeouts
        .first()
        .expect("timed table has a declaration")
        .clone();
    let persistence_id = format!("default:TimedTask:{entity_id}");
    let actor = EntityActor::with_persistence(
        "TimedTask",
        entity_id,
        table.clone(),
        serde_json::json!({}),
        BoxedEventStore::new(sim.clone()),
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let system = ActorSystem::new(format!("timeout-table-replay-{entity_id}"));
    let actor_ref = system.spawn(actor, entity_id);
    let before: EntityResponse = actor_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .expect("timed actor is ready");
    TimeoutHarness {
        actor_ref,
        table,
        sim,
        persistence_id,
        expected_timeout,
        reset_at: before
            .state
            .state_timeout_clock_reset_at
            .expect("Created established the timeout clock"),
        reset_version: before
            .state
            .state_timeout_clock_reset_version
            .expect("Created established the timeout clock version"),
    }
}

fn timeout_message(harness: &TimeoutHarness) -> EntityMsg {
    EntityMsg::Action {
        name: "TimeoutFail".to_string(),
        params: serde_json::json!({}),
        cross_entity_booleans: BTreeMap::new(),
        idempotency_key: None,
        state_timeout_precondition: Some(Box::new(StateTimeoutPrecondition {
            expected_timeout: harness.expected_timeout.clone(),
            expected_state: "Running".to_string(),
            expected_reset_at: Some(harness.reset_at),
            expected_reset_version: Some(harness.reset_version),
        })),
    }
}

async fn swap_while_first_append_is_delayed(harness: &TimeoutHarness, replacement: &str) {
    for _ in 0..64 {
        if harness.sim.pending_append_delays(&harness.persistence_id) == 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        harness.sim.pending_append_delays(&harness.persistence_id),
        0,
        "the first persistence attempt must be inside the injected delay"
    );
    *harness.table.write().expect("table lock") = TransitionTable::from_ioa_source(replacement);
    tokio::time::advance(Duration::from_secs(1)).await;
}

#[tokio::test(start_paused = true)]
async fn retry_rejects_a_timeout_declaration_removed_during_the_failed_append() {
    let seed = 221;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let harness = setup_timeout_actor(seed, "removed-declaration").await;
    harness
        .sim
        .inject_append_delay(&harness.persistence_id, Duration::from_secs(1));
    harness
        .sim
        .inject_concurrency_violations(&harness.persistence_id, 1);

    let timeout_attempt = harness
        .actor_ref
        .ask::<EntityResponse>(timeout_message(&harness), Duration::from_secs(5));
    let swap = swap_while_first_append_is_delayed(&harness, TIMED_WITHOUT_TIMEOUT_IOA);
    let (response, ()) = tokio::join!(timeout_attempt, swap);
    let response = response.expect("timeout attempt receives a replay-aware response");

    assert!(!response.success);
    assert_eq!(
        response.error.as_deref(),
        Some("state timeout precondition no longer matches")
    );
    assert_eq!(response.state.status, "Running");
    assert_eq!(
        harness.sim.dump_journal(&harness.persistence_id).len(),
        1,
        "the obsolete timeout must not append after replay sees the live table"
    );
}

#[tokio::test(start_paused = true)]
async fn retry_uses_one_live_table_for_replay_evaluation_and_commit() {
    let seed = 222;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let harness = setup_timeout_actor(seed, "changed-replay-effect").await;
    harness
        .sim
        .append(
            &harness.persistence_id,
            1,
            &[PersistenceEnvelope {
                sequence_nr: 0,
                event_type: "Observe".to_string(),
                payload: serde_json::json!({
                    "action": "Observe",
                    "from_status": "Running",
                    "to_status": "Running",
                    "timestamp": harness.reset_at,
                    "params": {},
                    "__temper_state_timeout_clock": {
                        "kind": "active",
                        "reset_at": harness.reset_at,
                        "reset_version": harness.reset_version
                    }
                }),
                metadata: EventMetadata {
                    event_id: sim_uuid(),
                    causation_id: sim_uuid(),
                    correlation_id: sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: harness.persistence_id.clone(),
                },
            }],
        )
        .await
        .expect("a competing replica commits Observe");
    harness
        .sim
        .inject_append_delay(&harness.persistence_id, Duration::from_secs(1));

    let timeout_attempt = harness
        .actor_ref
        .ask::<EntityResponse>(timeout_message(&harness), Duration::from_secs(5));
    let swap = swap_while_first_append_is_delayed(&harness, TIMED_WITH_CHANGED_REPLAY_EFFECT_IOA);
    let (response, ()) = tokio::join!(timeout_attempt, swap);
    let response = response.expect("retry succeeds under one live table snapshot");

    assert!(
        response.success,
        "retry must commit under the replacement table"
    );
    assert_eq!(response.state.status, "TimedOut");
    assert_eq!(response.state.counters.get("heartbeats"), Some(&1));
    assert_eq!(harness.sim.dump_journal(&harness.persistence_id).len(), 3);

    let replay_actor = EntityActor::with_persistence(
        "TimedTask",
        "changed-replay-effect",
        harness.table.clone(),
        serde_json::json!({}),
        BoxedEventStore::new(harness.sim.clone()),
        BackendLabel::Sim,
    )
    .with_tenant("default");
    let replay_system = ActorSystem::new("timeout-table-fresh-replay");
    let replay_ref = replay_system.spawn(replay_actor, "changed-replay-effect-fresh");
    let replayed: EntityResponse = replay_ref
        .ask(EntityMsg::GetState, Duration::from_secs(5))
        .await
        .expect("fresh actor replays the committed journal");
    assert_eq!(replayed.state.status, response.state.status);
    assert_eq!(replayed.state.counters, response.state.counters);
    assert_eq!(replayed.state.sequence_nr, response.state.sequence_nr);
}
