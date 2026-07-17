//! Deterministic fault regressions for state-timeout delivery.

use super::*;
use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::state::admission::AdmissionOutcome;
use crate::state::dispatch::effects::PostDispatchContext;
use crate::state::{AdmissionPermit, ServerState};
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

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

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

const TICKET_WITH_OLD_TIMEOUT_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "ExpiredByOld", "ExpiredByNew"]
initial = "Open"
allow_indefinite_states = ["ExpiredByOld", "ExpiredByNew"]

[[action]]
name = "OldTimeout"
kind = "internal"
from = ["Open"]
to = "ExpiredByOld"

[[action]]
name = "NewTimeout"
kind = "internal"
from = ["Open"]
to = "ExpiredByNew"

[[state_timeout]]
state = "Open"
after_seconds = 1
on_timeout = "OldTimeout"
"#;

const TICKET_WITH_CHANGED_TIMEOUT_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "ExpiredByOld", "ExpiredByNew"]
initial = "Open"
allow_indefinite_states = ["ExpiredByOld", "ExpiredByNew"]

[[action]]
name = "OldTimeout"
kind = "internal"
from = ["Open"]
to = "ExpiredByOld"

[[action]]
name = "NewTimeout"
kind = "internal"
from = ["Open"]
to = "ExpiredByNew"

[[state_timeout]]
state = "Open"
after_seconds = 1
on_timeout = "NewTimeout"
"#;

const TICKET_WITH_REMOVED_TIMEOUT_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "ExpiredByOld", "ExpiredByNew"]
initial = "Open"
allow_indefinite_states = ["Open", "ExpiredByOld", "ExpiredByNew"]

[[action]]
name = "OldTimeout"
kind = "internal"
from = ["Open"]
to = "ExpiredByOld"

[[action]]
name = "NewTimeout"
kind = "internal"
from = ["Open"]
to = "ExpiredByNew"
"#;

async fn setup_blocked_timeout(entity_id: &str) -> (ServerState, TenantId, AdmissionPermit) {
    let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        TICKET_CSDL.to_string(),
        &[("Ticket", TICKET_WITH_ADMITTED_TIMEOUT_IOA)],
    );
    let state = ServerState::from_registry(
        ActorSystem::new(format!("timeout-delivery-retry-{entity_id}")),
        registry,
    );
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
        .get_or_create_tenant_entity(&tenant, "Ticket", entity_id, serde_json::json!({}))
        .await
        .expect("create the timed ticket");
    assert_eq!(created.state.status, "Open");

    let response = state
        .get_tenant_entity_state(&tenant, "Ticket", entity_id)
        .await
        .expect("read the initial state");
    let agent_ctx = AgentContext::for_service("timeout-delivery-retry");
    let action_params = serde_json::json!({});
    let ctx = PostDispatchContext {
        tenant: &tenant,
        entity_type: "Ticket",
        entity_id,
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

    (state, tenant, held_permit)
}

async fn setup_hot_swappable_timeout(entity_id: &str) -> (ServerState, TenantId) {
    let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        TICKET_CSDL.to_string(),
        &[("Ticket", TICKET_WITH_OLD_TIMEOUT_IOA)],
    );
    let state = ServerState::from_registry(
        ActorSystem::new(format!("timeout-declaration-hotswap-{entity_id}")),
        registry,
    );
    let tenant = TenantId::default();

    let created = state
        .get_or_create_tenant_entity(&tenant, "Ticket", entity_id, serde_json::json!({}))
        .await
        .expect("create the timed ticket");
    assert_eq!(created.state.status, "Open");

    let agent_ctx = AgentContext::for_service("timeout-declaration-hotswap");
    let action_params = serde_json::json!({});
    let ctx = PostDispatchContext {
        tenant: &tenant,
        entity_type: "Ticket",
        entity_id,
        action: "__Created",
        agent_ctx: &agent_ctx,
        dispatch_idempotency_key: None,
        action_params: &action_params,
        await_integration: false,
    };
    state.arm_state_timeouts_if_needed(&ctx, &created);
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 1)]
    );

    (state, tenant)
}

fn hot_swap_ticket_timeout(state: &ServerState, ioa_source: &str) {
    let mut registry = state.registry.write().expect("registry lock");
    let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
    registry.register_tenant(
        "default",
        csdl,
        TICKET_CSDL.to_string(),
        &[("Ticket", ioa_source)],
    );
}

#[tokio::test(start_paused = true)]
async fn deferred_timeout_delivery_retries_without_traffic_or_restart() {
    let seed = 51;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let (state, tenant, held_permit) = setup_blocked_timeout("retry-after-deferred").await;

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
    let timeout_key = after
        .state
        .events
        .iter()
        .find(|event| event.action == "AssignAgent")
        .and_then(|event| event.idempotency_key.as_deref())
        .expect("timeout transition carries its internal idempotency key");
    assert!(
        timeout_key.starts_with("state-timeout:"),
        "timeout retries must use a distinct internal request identity"
    );
}

#[tokio::test(start_paused = true)]
async fn newer_transition_cancels_a_timeout_waiting_to_retry() {
    let seed = 52;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let (state, tenant, held_permit) = setup_blocked_timeout("cancel-deferred-retry").await;

    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }

    let agent_ctx = AgentContext::for_service("timeout-retry-cancellation");
    let closed = state
        .dispatch(DispatchCommand {
            tenant: &tenant,
            entity_type: "Ticket",
            entity_id: "cancel-deferred-retry",
            action: "Close",
            params: serde_json::json!({}),
            agent_ctx: &agent_ctx,
            await_integration: false,
            await_reactions: true,
        })
        .await
        .expect("the newer transition must succeed");
    assert!(closed.success);
    assert_eq!(closed.state.status, "Closed");

    drop(held_permit);
    tokio::time::advance(Duration::from_secs(5)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    let after = state
        .get_tenant_entity_state(&tenant, "Ticket", "cancel-deferred-retry")
        .await
        .expect("observe the state after the stale retry wakes");
    assert_eq!(after.state.status, "Closed");
    assert_eq!(
        after
            .state
            .events
            .iter()
            .filter(|event| event.action == "AssignAgent")
            .count(),
        0,
        "a stale delivery retry must not fire after ownership advances"
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 0)],
        "the cancelled retry must release its pending metric"
    );
}

#[tokio::test(start_paused = true)]
async fn changed_timeout_declaration_rebinds_an_armed_timer() {
    let seed = 53;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let (state, tenant) = setup_hot_swappable_timeout("changed-declaration").await;
    hot_swap_ticket_timeout(&state, TICKET_WITH_CHANGED_TIMEOUT_IOA);

    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        let current = state
            .get_tenant_entity_state(&tenant, "Ticket", "changed-declaration")
            .await
            .expect("ticket remains readable");
        if current.state.status != "Open" {
            break;
        }
    }

    let after = state
        .get_tenant_entity_state(&tenant, "Ticket", "changed-declaration")
        .await
        .expect("observe the current timeout declaration");
    assert_eq!(
        after.state.status, "ExpiredByNew",
        "a timer captured from an obsolete declaration must rebind to the current action"
    );
    assert_eq!(
        after
            .state
            .events
            .iter()
            .filter(|event| event.action == "OldTimeout")
            .count(),
        0,
        "the obsolete timeout action must never commit"
    );
}

#[tokio::test(start_paused = true)]
async fn removed_timeout_declaration_cancels_an_armed_timer() {
    let seed = 54;
    let (_guard, _clock, _id_gen) = install_deterministic_context(seed);
    let (state, tenant) = setup_hot_swappable_timeout("removed-declaration").await;
    hot_swap_ticket_timeout(&state, TICKET_WITH_REMOVED_TIMEOUT_IOA);

    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    let after = state
        .get_tenant_entity_state(&tenant, "Ticket", "removed-declaration")
        .await
        .expect("observe the entity after timeout removal");
    assert_eq!(
        after.state.status, "Open",
        "removing a timeout declaration must terminally cancel its armed task"
    );
    assert_eq!(
        after
            .state
            .events
            .iter()
            .filter(|event| event.action == "OldTimeout")
            .count(),
        0,
        "the removed timeout action must never commit"
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 0)],
        "declaration removal must release pending ownership"
    );
}
