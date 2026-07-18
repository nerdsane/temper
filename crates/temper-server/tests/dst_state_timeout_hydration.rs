//! Deterministic restart regressions for durable `[[state_timeout]]` behavior.

use temper_runtime::ActorSystem;
use temper_runtime::persistence::{EventMetadata, EventStore, PersistenceEnvelope};
use temper_runtime::scheduler::{install_deterministic_context, sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_server::entity_actor::EntityEvent;
use temper_server::registry::SpecRegistry;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

#[path = "dst_state_timeout_hydration/authority.rs"]
mod authority;
#[path = "dst_state_timeout_hydration/reset_on_hotswap.rs"]
mod reset_on_hotswap;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");

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

[[action]]
name = "Heartbeat"
kind = "input"
from = ["Running"]
to = "Running"

[[state_timeout]]
state = "Running"
after_seconds = 60
on_timeout = "TimeoutFail"
reset_on = ["Heartbeat"]
"#;

const UNTIMED_LAZY_TASK_IOA: &str = r#"
[automaton]
name = "LazyTask"
states = ["Idle"]
initial = "Idle"
allow_indefinite_states = ["Idle"]
"#;

fn persisted_event(
    persistence_id: &str,
    sequence_nr: u64,
    event: EntityEvent,
) -> PersistenceEnvelope {
    PersistenceEnvelope {
        sequence_nr,
        event_type: event.action.clone(),
        payload: serde_json::to_value(&event).expect("event serialization"),
        metadata: EventMetadata {
            event_id: sim_uuid(),
            causation_id: sim_uuid(),
            correlation_id: sim_uuid(),
            timestamp: event.timestamp,
            actor_id: persistence_id.to_string(),
        },
    }
}

fn restarted_server(store: SimEventStore) -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[
            ("TimedTask", TIMED_TASK_IOA),
            ("LazyTask", UNTIMED_LAZY_TASK_IOA),
        ],
    );

    let system = ActorSystem::new("dst-state-timeout-restart");
    let mut state = ServerState::from_registry(system, registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    state
}

async fn seed_running_entity(
    store: &SimEventStore,
    persistence_id: &str,
    entity_id: &str,
    entered_running_at: chrono::DateTime<chrono::Utc>,
) {
    let created = EntityEvent {
        action: "Created".to_string(),
        from_status: String::new(),
        to_status: "Idle".to_string(),
        timestamp: entered_running_at,
        params: serde_json::json!({"Id": entity_id}),
        idempotency_key: None,
    };
    let started = EntityEvent {
        action: "Start".to_string(),
        from_status: "Idle".to_string(),
        to_status: "Running".to_string(),
        timestamp: entered_running_at,
        params: serde_json::json!({}),
        idempotency_key: None,
    };
    store
        .append(
            persistence_id,
            0,
            &[
                persisted_event(persistence_id, 1, created),
                persisted_event(persistence_id, 2, started),
            ],
        )
        .await
        .expect("seed durable timed state");

    // Current actor snapshots intentionally omit the hot event deque, but
    // retain the exact timeout clock anchor as dedicated scheduler metadata.
    let snapshot = serde_json::json!({
        "entity_type": "TimedTask",
        "entity_id": entity_id,
        "status": "Running",
        "item_count": 0,
        "counters": {},
        "booleans": {},
        "lists": {},
        "fields": {"Id": entity_id, "Status": "Running"},
        "state_timeout_clock_reset_at": entered_running_at,
        "state_timeout_clock_reset_version": 2,
        "total_event_count": 2,
        "events_since_snapshot": 0,
        "last_snapshot_sequence_nr": 2,
        "sequence_nr": 2,
        "processed_idempotency_keys": {},
    });
    store
        .save_snapshot(
            persistence_id,
            2,
            &serde_json::to_vec(&snapshot).expect("snapshot serialization"),
        )
        .await
        .expect("seed current snapshot with timeout anchor");
}

#[tokio::test(start_paused = true)]
async fn replayed_remote_reset_replaces_the_cancelled_local_deadline() {
    let (_guard, clock, _ids) = install_deterministic_context(221);
    let store = SimEventStore::no_faults(221);
    let tenant = TenantId::default();
    let entity_id = "timed-task-remote-reset";
    let persistence_id = format!("default:TimedTask:{entity_id}");
    let entered_running_at = sim_now();
    seed_running_entity(&store, &persistence_id, entity_id, entered_running_at).await;

    let state = restarted_server(store.clone());
    state.populate_index_from_store(&tenant).await;
    for _ in 0..64 {
        if state.state_timeout_tracker.pending_snapshot() == vec![("TimedTask".to_string(), 1)] {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedTask".to_string(), 1)]
    );

    tokio::time::advance(std::time::Duration::from_secs(20)).await;
    clock.advance_by(200);
    let reset_at = sim_now();
    let heartbeat = EntityEvent {
        action: "Heartbeat".to_string(),
        from_status: "Running".to_string(),
        to_status: "Running".to_string(),
        timestamp: reset_at,
        params: serde_json::json!({}),
        idempotency_key: None,
    };
    store
        .append(
            &persistence_id,
            2,
            &[persisted_event(&persistence_id, 3, heartbeat)],
        )
        .await
        .expect("competing replica commits a same-state reset");

    tokio::time::advance(std::time::Duration::from_secs(40)).await;
    clock.advance_by(400);
    for _ in 0..128 {
        tokio::task::yield_now().await;
        let current = state
            .get_tenant_entity_state(&tenant, "TimedTask", entity_id)
            .await
            .expect("timed entity remains readable");
        if current.state.sequence_nr == 3
            && current.state.status == "Running"
            && state.state_timeout_tracker.pending_snapshot() == vec![("TimedTask".to_string(), 1)]
        {
            break;
        }
    }
    let reconciled = state
        .get_tenant_entity_state(&tenant, "TimedTask", entity_id)
        .await
        .expect("remote reset is replayed at the old deadline");
    assert_eq!(reconciled.state.status, "Running");
    assert_eq!(reconciled.state.sequence_nr, 3);
    assert_eq!(
        reconciled.state.state_timeout_clock_reset_at,
        Some(reset_at)
    );
    assert_eq!(reconciled.state.state_timeout_clock_reset_version, Some(3));
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedTask".to_string(), 1)],
        "the stale local fire must hand ownership to the replayed remote reset"
    );

    tokio::time::advance(std::time::Duration::from_secs(20)).await;
    clock.advance_by(200);
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if state.state_timeout_tracker.pending_snapshot() == vec![("TimedTask".to_string(), 0)] {
            break;
        }
    }
    let timed_out = state
        .get_tenant_entity_state(&tenant, "TimedTask", entity_id)
        .await
        .expect("replacement deadline completes");
    assert_eq!(timed_out.state.status, "TimedOut");
    assert_eq!(timed_out.state.sequence_nr, 4);
    assert_eq!(
        store
            .dump_journal(&persistence_id)
            .last()
            .map(|event| event.event_type.as_str()),
        Some("TimeoutFail")
    );
}

async fn seed_lazy_entity(store: &SimEventStore, entity_id: &str) {
    let persistence_id = format!("default:LazyTask:{entity_id}");
    let created = EntityEvent {
        action: "Created".to_string(),
        from_status: String::new(),
        to_status: "Idle".to_string(),
        timestamp: sim_now(),
        params: serde_json::json!({"Id": entity_id}),
        idempotency_key: None,
    };
    store
        .append(
            &persistence_id,
            0,
            &[persisted_event(&persistence_id, 1, created)],
        )
        .await
        .expect("seed durable non-timed state");
}

#[tokio::test]
async fn overdue_snapshot_timeout_fires_after_restart_without_an_unrelated_dispatch() {
    let (_guard, _clock, _ids) = install_deterministic_context(203);
    let store = SimEventStore::no_faults(203);
    let tenant = TenantId::default();
    let entity_id = "timed-task-overdue";
    let persistence_id = format!("default:TimedTask:{entity_id}");
    let entered_running_at = sim_now() - chrono::Duration::seconds(61);
    seed_running_entity(&store, &persistence_id, entity_id, entered_running_at).await;
    seed_lazy_entity(&store, "lazy-control").await;

    let state = restarted_server(store);
    state.populate_index_from_store(&tenant).await;
    assert_eq!(
        state.active_entity_count(),
        2,
        "index-only startup must discover timed and non-timed entities"
    );
    assert_eq!(
        state.active_actor_count(),
        1,
        "index-only startup must activate only the persisted timed entity"
    );

    let mut observed_status = String::new();
    for _ in 0..32 {
        tokio::task::yield_now().await;
        observed_status = state
            .get_tenant_entity_state(&tenant, "TimedTask", entity_id)
            .await
            .expect("hydrated entity remains readable")
            .state
            .status;
        if observed_status == "TimedOut" {
            break;
        }
    }

    assert_eq!(
        observed_status, "TimedOut",
        "a persisted overdue state_timeout must fire after restart without waiting for unrelated action traffic"
    );
}

#[tokio::test(start_paused = true)]
async fn budgeted_snapshot_timeout_is_rearmed_without_firing_early_or_late() {
    let (_guard, _clock, _ids) = install_deterministic_context(204);
    let store = SimEventStore::no_faults(204);
    let tenant = TenantId::default();
    let entity_id = "timed-task-budgeted";
    let persistence_id = format!("default:TimedTask:{entity_id}");
    let entered_running_at = sim_now() - chrono::Duration::seconds(17);
    seed_running_entity(&store, &persistence_id, entity_id, entered_running_at).await;

    let state = restarted_server(store);
    state
        .get_or_spawn_tenant_actor(&tenant, "TimedTask", entity_id)
        .expect("restart spawns the durable entity actor");

    // Hold the spawned actor and its readiness task off CPU for five logical
    // seconds. The recovered timer must charge this queue/readiness interval
    // rather than moving the durable deadline five seconds later.
    tokio::time::advance(std::time::Duration::from_secs(5)).await;

    for _ in 0..32 {
        if state.state_timeout_tracker.pending_snapshot() == vec![("TimedTask".to_string(), 1)] {
            break;
        }
        tokio::task::yield_now().await;
    }

    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedTask".to_string(), 1)],
        "restart hydration must restore one live timer for a budgeted timeout"
    );
    let observed = state
        .get_tenant_entity_state(&tenant, "TimedTask", entity_id)
        .await
        .expect("hydrated entity remains readable");
    assert_eq!(
        observed.state.status, "Running",
        "a timeout with remaining budget must not fire during hydration"
    );

    tokio::time::advance(std::time::Duration::from_millis(37_999)).await;
    tokio::task::yield_now().await;
    let before_deadline = state
        .get_tenant_entity_state(&tenant, "TimedTask", entity_id)
        .await
        .expect("entity remains readable before the recovered deadline");
    assert_eq!(
        before_deadline.state.status, "Running",
        "restart recovery must preserve the remaining timeout budget"
    );

    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    let mut observed_status = String::new();
    for _ in 0..32 {
        tokio::task::yield_now().await;
        observed_status = state
            .get_tenant_entity_state(&tenant, "TimedTask", entity_id)
            .await
            .expect("entity remains readable at the recovered deadline")
            .state
            .status;
        if observed_status == "TimedOut" {
            break;
        }
    }
    assert_eq!(
        observed_status, "TimedOut",
        "the timeout must fire when its persisted remaining budget is consumed"
    );
}
