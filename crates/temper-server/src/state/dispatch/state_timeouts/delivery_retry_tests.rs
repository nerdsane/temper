//! Deterministic fault regressions for state-timeout delivery.

use super::*;
use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::state::ServerState;
use crate::state::admission::AdmissionOutcome;
use crate::state::dispatch::effects::PostDispatchContext;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;

const TICKET_CSDL: &str = include_str!("../../../../../../test-fixtures/specs/model.csdl.xml");

const TICKET_WITH_ADMITTED_TIMEOUT_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "InProgress", "WaitingOnCustomer", "Resolved", "Closed"]
initial = "Open"
allow_indefinite_states = ["InProgress", "WaitingOnCustomer", "Resolved", "Closed"]

[[state]]
name = "replies"
type = "counter"
initial = "0"

[[state]]
name = "customer_responded"
type = "bool"
initial = "false"

[[action]]
name = "AssignAgent"
kind = "input"
from = ["Open"]
to = "InProgress"

[[state_timeout]]
state = "Open"
after_seconds = 1
on_timeout = "AssignAgent"

[admission]
max_concurrent_creates = 1
max_concurrent_actions = { "AssignAgent" = 1 }
queue_depth = 1
queue_timeout_seconds = 0
"#;

#[tokio::test(start_paused = true)]
async fn deferred_timeout_delivery_retries_without_traffic_or_restart() {
    let seed = 51;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        TICKET_CSDL.to_string(),
        &[("Ticket", TICKET_WITH_ADMITTED_TIMEOUT_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("timeout-delivery-retry"), registry);
    let tenant = TenantId::default();

    let admission = state
        .admission_caps_for(&tenant, "Ticket")
        .expect("timeout action has an admission cap");
    let held_permit = match state
        .admission
        .try_acquire_with_caps(&tenant, "Ticket", "AssignAgent", Some(&admission))
        .await
    {
        AdmissionOutcome::Granted(permit) => permit,
        other => panic!("first admission acquisition must be granted: {other:?}"),
    };

    let created = state
        .get_or_create_tenant_entity(
            &tenant,
            "Ticket",
            "retry-after-deferred",
            serde_json::json!({}),
        )
        .await
        .expect("create the timed ticket");
    assert_eq!(created.state.status, "Open");

    let response = state
        .get_tenant_entity_state(&tenant, "Ticket", "retry-after-deferred")
        .await
        .expect("read the initial state");
    let agent_ctx = AgentContext::for_service("timeout-delivery-retry");
    let action_params = serde_json::json!({});
    let ctx = PostDispatchContext {
        tenant: &tenant,
        entity_type: "Ticket",
        entity_id: "retry-after-deferred",
        action: "__Created",
        agent_ctx: &agent_ctx,
        dispatch_idempotency_key: None,
        action_params: &action_params,
        await_integration: false,
    };
    state.arm_state_timeouts_if_needed(&ctx, &response);
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 1)],
        "one timeout must own the durable deadline"
    );

    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }

    // The first delivery is deterministically Deferred because the only
    // AssignAgent permit is still held. Restoring capacity is not entity
    // traffic and must let the same timeout finish without a restart.
    drop(held_permit);
    tokio::time::advance(Duration::from_secs(5)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    let after = state
        .get_tenant_entity_state(&tenant, "Ticket", "retry-after-deferred")
        .await
        .expect("observe the state after capacity is restored");
    assert_eq!(
        after.state.status, "InProgress",
        "a transient delivery failure must not consume the timeout"
    );
    assert_eq!(
        after
            .state
            .events
            .iter()
            .filter(|event| event.action == "AssignAgent")
            .count(),
        1,
        "retrying the timeout must commit exactly one transition"
    );
}
