//! Restart regressions for declaration changes that add or remove `reset_on`.

use super::*;
use temper_server::EntityResponse;
use temper_server::request_context::AgentContext;

const TIMED_TASK_WITHOUT_RESET_IOA: &str = r#"
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
name = "Heartbeat"
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
"#;

fn server_with_timeout_spec(
    store: SimEventStore,
    system_name: &str,
    ioa_source: &str,
) -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("TimedTask", ioa_source)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state
}

fn hot_swap_timeout(state: &ServerState, ioa_source: &str) {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    state
        .registry
        .write()
        .expect("registry lock")
        .register_tenant(
            "default",
            csdl,
            CSDL_XML.to_string(),
            &[("TimedTask", ioa_source)],
        );
}

async fn dispatch(state: &ServerState, entity_id: &str, action: &str) -> EntityResponse {
    state
        .dispatch_tenant_action(
            &TenantId::default(),
            "TimedTask",
            entity_id,
            action,
            serde_json::json!({}),
            &AgentContext::for_service("reset-on-hotswap-test"),
        )
        .await
        .unwrap_or_else(|error| panic!("dispatch {action}: {error}"))
}

async fn wait_for_pending_timeout(state: &ServerState) {
    for _ in 0..128 {
        if state.state_timeout_tracker.pending_snapshot() == vec![("TimedTask".to_string(), 1)] {
            return;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedTask".to_string(), 1)],
        "the current declaration must own exactly one timer"
    );
}

fn simulate_abrupt_process_loss(state: ServerState, entity_id: &str) {
    let tenant = TenantId::default();
    state
        .state_timeout_tracker
        .forget(&tenant, "TimedTask", entity_id);
    let persistence_id = format!("default:TimedTask:{entity_id}");
    if let Some(actor) = state
        .actor_registry
        .read()
        .expect("actor registry lock")
        .get(&persistence_id)
        .cloned()
    {
        actor.stop().expect("stop the pre-crash actor");
    }
    drop(state);
}

async fn status(state: &ServerState, entity_id: &str) -> EntityResponse {
    state
        .get_tenant_entity_state(&TenantId::default(), "TimedTask", entity_id)
        .await
        .expect("read the restarted timed entity")
}

async fn assert_single_timeout(store: &SimEventStore, entity_id: &str) {
    let persistence_id = format!("default:TimedTask:{entity_id}");
    let journal = store.dump_journal(&persistence_id);
    assert_eq!(
        journal
            .iter()
            .filter(|event| event.event_type == "TimeoutFail")
            .count(),
        1,
        "restart must commit exactly one timeout delivery"
    );
    assert_eq!(journal.len(), 4, "Created, Start, Heartbeat, TimeoutFail");
}

#[tokio::test(start_paused = true)]
async fn removing_reset_on_preserves_the_committed_reset_deadline_across_restart() {
    let seed = 224;
    let (_guard, clock, _ids) = install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let entity_id = "remove-reset-on-restart";
    let persistence_id = format!("default:TimedTask:{entity_id}");

    let state = server_with_timeout_spec(store.clone(), "remove-reset-live", TIMED_TASK_IOA);
    state
        .get_or_create_tenant_entity(
            &TenantId::default(),
            "TimedTask",
            entity_id,
            serde_json::json!({}),
        )
        .await
        .expect("create the timed entity");
    let started = dispatch(&state, entity_id, "Start").await;
    let entered_at = started.state.state_timeout_clock_reset_at;
    assert!(entered_at.is_some());

    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    clock.advance_by(300);
    let heartbeat = dispatch(&state, entity_id, "Heartbeat").await;
    let reset_at = heartbeat.state.state_timeout_clock_reset_at;
    assert!(
        reset_at > entered_at,
        "Heartbeat must durably reset the clock"
    );
    assert_eq!(heartbeat.state.state_timeout_clock_reset_version, Some(3));

    hot_swap_timeout(&state, TIMED_TASK_WITHOUT_RESET_IOA);
    wait_for_pending_timeout(&state).await;
    assert!(
        store
            .load_snapshot(&persistence_id)
            .await
            .expect("snapshot lookup")
            .is_none(),
        "the restart must recover from the unsnapshotted journal tail"
    );
    simulate_abrupt_process_loss(state, entity_id);

    let restarted = server_with_timeout_spec(
        store.clone(),
        "remove-reset-restarted",
        TIMED_TASK_WITHOUT_RESET_IOA,
    );
    restarted
        .populate_index_from_store(&TenantId::default())
        .await;
    wait_for_pending_timeout(&restarted).await;
    let hydrated = status(&restarted, entity_id).await;
    assert_eq!(hydrated.state.state_timeout_clock_reset_at, reset_at);
    assert_eq!(hydrated.state.state_timeout_clock_reset_version, Some(3));

    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    clock.advance_by(300);
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        status(&restarted, entity_id).await.state.status,
        "Running",
        "removing reset_on must not move the established Heartbeat deadline backward"
    );

    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    clock.advance_by(300);
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if status(&restarted, entity_id).await.state.status == "TimedOut" {
            break;
        }
    }
    let timed_out = status(&restarted, entity_id).await;
    assert_eq!(timed_out.state.status, "TimedOut");
    assert_eq!(timed_out.state.sequence_nr, 4);
    assert_single_timeout(&store, entity_id).await;
}

#[tokio::test(start_paused = true)]
async fn adding_reset_on_does_not_retroactively_reset_a_committed_event_after_restart() {
    let seed = 225;
    let (_guard, clock, _ids) = install_deterministic_context(seed);
    let store = SimEventStore::no_faults(seed);
    let entity_id = "add-reset-on-restart";
    let persistence_id = format!("default:TimedTask:{entity_id}");

    let state = server_with_timeout_spec(
        store.clone(),
        "add-reset-live",
        TIMED_TASK_WITHOUT_RESET_IOA,
    );
    state
        .get_or_create_tenant_entity(
            &TenantId::default(),
            "TimedTask",
            entity_id,
            serde_json::json!({}),
        )
        .await
        .expect("create the timed entity");
    let started = dispatch(&state, entity_id, "Start").await;
    let entered_at = started.state.state_timeout_clock_reset_at;
    assert!(entered_at.is_some());

    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    clock.advance_by(300);
    let heartbeat = dispatch(&state, entity_id, "Heartbeat").await;
    assert_eq!(
        heartbeat.state.state_timeout_clock_reset_at, entered_at,
        "Heartbeat was not a reset under the table that committed it"
    );
    assert_eq!(heartbeat.state.state_timeout_clock_reset_version, Some(2));

    hot_swap_timeout(&state, TIMED_TASK_IOA);
    wait_for_pending_timeout(&state).await;
    assert!(
        store
            .load_snapshot(&persistence_id)
            .await
            .expect("snapshot lookup")
            .is_none(),
        "the restart must recover from the unsnapshotted journal tail"
    );
    simulate_abrupt_process_loss(state, entity_id);

    let restarted = server_with_timeout_spec(store.clone(), "add-reset-restarted", TIMED_TASK_IOA);
    restarted
        .populate_index_from_store(&TenantId::default())
        .await;
    wait_for_pending_timeout(&restarted).await;
    let hydrated = status(&restarted, entity_id).await;
    assert_eq!(hydrated.state.state_timeout_clock_reset_at, entered_at);
    assert_eq!(hydrated.state.state_timeout_clock_reset_version, Some(2));

    tokio::time::advance(std::time::Duration::from_secs(30)).await;
    clock.advance_by(300);
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if status(&restarted, entity_id).await.state.status == "TimedOut" {
            break;
        }
    }
    let timed_out = status(&restarted, entity_id).await;
    assert_eq!(
        timed_out.state.status, "TimedOut",
        "adding reset_on must not retroactively reinterpret the committed Heartbeat"
    );
    assert_eq!(timed_out.state.sequence_nr, 4);
    assert_single_timeout(&store, entity_id).await;
}
