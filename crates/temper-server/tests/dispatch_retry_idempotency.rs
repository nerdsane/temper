//! Regression tests for dispatch retry idempotency around post-dispatch effects.

use std::sync::Arc;
use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerEventStore, ServerState};
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
    let store = ServerEventStore::Sim(sim_store.clone(), None);

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
    state.event_store = Some(Arc::new(store));
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
