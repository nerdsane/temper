//! Regression tests for dispatch retry idempotency around post-dispatch effects.

use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");

const TASK_WITH_TIMEOUT_IOA: &str = r#"
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

fn build_state_with_sim_store(seed: u64) -> (ServerState, SimEventStore) {
    let sim_store = SimEventStore::no_faults(seed);

    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("TimedTask", TASK_WITH_TIMEOUT_IOA)],
    );

    let system = ActorSystem::new("dispatch-retry-idempotency");
    let mut state = ServerState::from_registry(system, registry);
    state.set_storage_stack(StorageStack::from_sim(sim_store.clone(), None));
    state.action_dispatch_timeout = Duration::from_millis(5);
    (state, sim_store)
}

#[tokio::test]
async fn retry_after_dropped_reply_replays_success_response_and_runs_effects_without_header() {
    let (_guard, _clock, _ids) = install_deterministic_context(48);
    let (state, sim_store) = build_state_with_sim_store(48);
    let tenant = TenantId::default();
    let entity_id = "timed-task-1";
    let persistence_id = format!("default:TimedTask:{entity_id}");

    state
        .get_or_create_tenant_entity(
            &tenant,
            "TimedTask",
            entity_id,
            serde_json::json!({"Id": entity_id}),
        )
        .await
        .expect("entity creation succeeds");

    sim_store.inject_append_delay(&persistence_id, Duration::from_millis(25));

    let response = state
        .dispatch_tenant_action(
            &tenant,
            "TimedTask",
            entity_id,
            "Start",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("dispatch should recover the timed-out first reply");

    assert!(
        response.success,
        "retry should return the cached successful Start response, got {:?}",
        response.error
    );
    assert_eq!(response.state.status, "Running");

    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("TimedTask".to_string(), 1)],
        "post-dispatch effects from the successful Start transition must arm the state_timeout"
    );
}

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn order_state(storage: Option<SimEventStore>) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("retry-reply-state"), registry);
    if let Some(store) = storage {
        state.set_storage_stack(StorageStack::from_sim(store, None));
    }
    state
}

fn keyed(key: &str) -> AgentContext {
    AgentContext {
        idempotency_key: Some(temper_server::idempotency::IdempotencyKey::Request(
            key.to_string(),
        )),
        ..AgentContext::default()
    }
}

/// A replayed reply must carry the state its own action produced. Without a
/// journal the sequence number never moves, so it cannot tell that another
/// action ran after the one being retried.
#[tokio::test]
async fn retry_after_another_action_gets_its_own_state_without_a_journal() {
    let (_guard, _clock, _ids) = install_deterministic_context(49);
    let state = order_state(None);
    let tenant = TenantId::default();
    let add = serde_json::json!({"ProductId": "p1", "Quantity": 1});

    let first = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "o-1",
            "AddItem",
            add.clone(),
            &keyed("add-1"),
        )
        .await
        .expect("AddItem dispatches");
    assert!(first.success, "AddItem failed: {:?}", first.error);
    let submitted = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "o-1",
            "SubmitOrder",
            serde_json::json!({}),
            &keyed("submit-1"),
        )
        .await
        .expect("SubmitOrder dispatches");
    assert!(
        submitted.success,
        "SubmitOrder failed: {:?}",
        submitted.error
    );
    assert_eq!(submitted.state.status, "Submitted");

    let retried = state
        .dispatch_tenant_action(&tenant, "Order", "o-1", "AddItem", add, &keyed("add-1"))
        .await
        .expect("the AddItem retry dispatches");
    assert!(retried.success);
    assert_eq!(
        retried.state.status, "Draft",
        "a retried AddItem replies with the state AddItem produced, not SubmitOrder's"
    );
    assert_eq!(
        retried
            .state
            .events
            .back()
            .map(|event| event.action.as_str()),
        Some("AddItem")
    );
}

/// A state restored from a snapshot keeps its sequence number but has no
/// recent events; a retry then needs the cached reply's event metadata.
#[tokio::test]
async fn retry_after_passivation_keeps_its_event_metadata() {
    let (_guard, _clock, _ids) = install_deterministic_context(50);
    let store = SimEventStore::no_faults(50);
    let state = order_state(Some(store));
    let tenant = TenantId::default();
    let add = serde_json::json!({"ProductId": "p1", "Quantity": 1});

    let first = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "o-2",
            "AddItem",
            add.clone(),
            &keyed("add-2"),
        )
        .await
        .expect("AddItem dispatches");
    assert!(first.success, "AddItem failed: {:?}", first.error);

    let actor_key = format!("{tenant}:Order:o-2");
    state.last_accessed.write().unwrap().insert(
        actor_key.clone(),
        temper_runtime::scheduler::sim_now() - chrono::Duration::seconds(600),
    );
    state.passivate_idle_actors().await;
    assert!(
        !state
            .actor_registry
            .read()
            .unwrap()
            .contains_key(&actor_key)
    );

    let retried = state
        .dispatch_tenant_action(&tenant, "Order", "o-2", "AddItem", add, &keyed("add-2"))
        .await
        .expect("the AddItem retry dispatches");
    assert!(retried.success);
    let last = retried
        .state
        .events
        .back()
        .expect("the replayed reply keeps its recent event metadata");
    assert_eq!(last.action, "AddItem");
    assert_eq!(last.from_status, "Draft");
}

/// A retry that arrives while its own event is still the newest gets the live
/// state in full, including the durable key set the cached copy leaves out.
#[tokio::test]
async fn immediate_retry_gets_the_live_state_in_full() {
    let (_guard, _clock, _ids) = install_deterministic_context(51);
    let state = order_state(Some(SimEventStore::no_faults(51)));
    let tenant = TenantId::default();
    let add = serde_json::json!({"ProductId": "p1", "Quantity": 1});

    let first = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "o-3",
            "AddItem",
            add.clone(),
            &keyed("add-3"),
        )
        .await
        .expect("AddItem dispatches");
    assert!(first.success, "AddItem failed: {:?}", first.error);

    let retried = state
        .dispatch_tenant_action(&tenant, "Order", "o-3", "AddItem", add, &keyed("add-3"))
        .await
        .expect("the AddItem retry dispatches");
    assert!(retried.success);
    assert_eq!(
        retried.state.item_count, first.state.item_count,
        "not applied twice"
    );
    assert!(
        retried
            .state
            .processed_idempotency_keys
            .contains_key("add-3")
    );
    assert_eq!(
        retried.state.events.back().map(|event| &event.params),
        first.state.events.back().map(|event| &event.params)
    );
}
