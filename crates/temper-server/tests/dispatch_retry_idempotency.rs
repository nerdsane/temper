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

#[tokio::test(start_paused = true)]
async fn explicit_idempotency_key_does_not_swallow_state_timeout() {
    let (_guard, _clock, _ids) = install_deterministic_context(49);
    let (state, _sim_store) = build_state_with_sim_store(49);
    let tenant = TenantId::default();
    let entity_id = "timed-task-explicit-idempotency";

    state
        .get_or_create_tenant_entity(
            &tenant,
            "TimedTask",
            entity_id,
            serde_json::json!({"Id": entity_id}),
        )
        .await
        .expect("entity creation succeeds");

    let mut agent_ctx = AgentContext::for_service("idempotent-caller");
    agent_ctx.idempotency_key = Some("start-request-1".to_string());
    let started = state
        .dispatch_tenant_action(
            &tenant,
            "TimedTask",
            entity_id,
            "Start",
            serde_json::json!({}),
            &agent_ctx,
        )
        .await
        .expect("Start succeeds with an explicit idempotency key");
    assert_eq!(started.state.status, "Running");
    assert!(
        started
            .state
            .processed_idempotency_keys
            .contains_key("start-request-1"),
        "the initiating action must retain its caller idempotency key"
    );

    tokio::time::advance(Duration::from_secs(60)).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }

    let timed_out = state
        .get_tenant_entity_state(&tenant, "TimedTask", entity_id)
        .await
        .expect("timed entity remains readable");
    assert_eq!(
        timed_out.state.status, "TimedOut",
        "the internal timeout must not be deduplicated against the caller's Start request"
    );
    assert_eq!(
        timed_out.state.sequence_nr, 3,
        "Created, Start, and TimeoutFail must each commit exactly once"
    );
}
