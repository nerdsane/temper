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

#[tokio::test(start_paused = true)]
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

    let pause = sim_store.inject_postcommit_append_pause(&persistence_id);
    let agent_context = AgentContext::default();
    let dispatch = state.dispatch_tenant_action(
        &tenant,
        "TimedTask",
        entity_id,
        "Start",
        serde_json::json!({}),
        &agent_context,
    );
    let advance_past_timeout = async {
        pause.wait_until_reached().await;
        tokio::time::advance(Duration::from_millis(6)).await;
        pause.resume();
        tokio::time::advance(Duration::from_secs(1)).await;
    };
    let (response, ()) = tokio::join!(dispatch, advance_past_timeout);
    let response = response.expect("dispatch should recover the timed-out first reply");

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
async fn field_update_retry_after_post_commit_timeout_appends_once() {
    let (_guard, _clock, _ids) = install_deterministic_context(238);
    let (state, sim_store) = build_state_with_sim_store(238);
    let tenant = TenantId::default();
    let entity_id = "timed-task-field-update";
    let persistence_id = format!("default:TimedTask:{entity_id}");

    state
        .get_or_create_tenant_entity(
            &tenant,
            "TimedTask",
            entity_id,
            serde_json::json!({"Id": entity_id, "Title": "before"}),
        )
        .await
        .expect("entity creation succeeds");

    // The sim store commits before reaching this one-shot barrier. The first ask
    // therefore times out after durability but before the actor can reply; the
    // dispatch retry must reuse one token and observe the committed update.
    let pause = sim_store.inject_postcommit_append_pause(&persistence_id);
    let update = state.update_tenant_entity_fields(
        &tenant,
        "TimedTask",
        entity_id,
        serde_json::json!({"Title": "after"}),
        false,
        Some("patch-post-commit-timeout".to_string()),
    );
    let advance_past_timeout = async {
        pause.wait_until_reached().await;
        tokio::time::advance(Duration::from_millis(6)).await;
        pause.resume();
        tokio::time::advance(Duration::from_secs(1)).await;
    };
    let (response, ()) = tokio::join!(update, advance_past_timeout);
    let response = response.expect("retry should return the committed field update");

    assert!(response.success);
    assert_eq!(response.state.fields["Title"], "after");
    assert_eq!(
        sim_store.dump_journal(&persistence_id).len(),
        2,
        "Create plus exactly one private field-update event must be durable"
    );
}
