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

const TICKET_WITH_REPLACEMENT_TIMEOUT_IOA: &str = r#"
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
on_timeout = "Close"

[admission]
max_concurrent_creates = 1
max_concurrent_actions = { "AssignAgent" = 1 }
queue_depth = 1
queue_timeout_seconds = 0
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
        actor_uid: None,
    };
    state.arm_state_timeouts_if_needed(&ctx, &response);
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 1)],
        "one timeout must own the durable deadline"
    );

    (state, tenant, held_permit)
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
async fn removed_declaration_cancels_a_timeout_waiting_to_retry() {
    let seed = 55;
    let (_guard, clock, _id_gen) = install_deterministic_context(seed);
    let (state, tenant, held_permit) = setup_blocked_timeout("remove-deferred-retry").await;

    clock.advance_by(10);
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }

    hot_swap_ticket_timeout(&state, TICKET_WITH_REMOVED_TIMEOUT_IOA);
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if state.state_timeout_tracker.pending_snapshot() == vec![("Ticket".to_string(), 0)] {
            break;
        }
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 0)],
        "table removal must wake and cancel a task inside retry backoff"
    );

    drop(held_permit);
    clock.advance_by(50);
    tokio::time::advance(Duration::from_secs(5)).await;
    let after = state
        .get_tenant_entity_state(&tenant, "Ticket", "remove-deferred-retry")
        .await
        .expect("observe the entity after retry cancellation");
    assert_eq!(after.state.status, "Open");
    assert_eq!(
        after
            .state
            .events
            .iter()
            .filter(|event| event.action == "AssignAgent")
            .count(),
        0,
        "the removed timeout action must not retry after capacity returns"
    );
}

#[tokio::test(start_paused = true)]
async fn replacement_declaration_rebinds_a_timeout_waiting_to_retry() {
    let seed = 58;
    let (_guard, clock, _id_gen) = install_deterministic_context(seed);
    let (state, tenant, held_permit) = setup_blocked_timeout("replace-deferred-retry").await;

    clock.advance_by(10);
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }

    hot_swap_ticket_timeout(&state, TICKET_WITH_REPLACEMENT_TIMEOUT_IOA);
    let key = EntityKey::new(&tenant, "Ticket", "replace-deferred-retry");
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if state.state_timeout_tracker.current_generation(&key) != 1 {
            break;
        }
    }
    assert_ne!(
        state.state_timeout_tracker.current_generation(&key),
        1,
        "the table-version signal must rebind ownership during retry backoff"
    );
    clock.advance_by(10);
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    let rebound = state
        .get_tenant_entity_state(&tenant, "Ticket", "replace-deferred-retry")
        .await
        .expect("observe the replacement timeout");
    assert_eq!(
        rebound.state.status, "Closed",
        "the current declaration must replace an obsolete task inside retry backoff"
    );
    assert_eq!(
        rebound
            .state
            .events
            .iter()
            .filter(|event| event.action == "AssignAgent")
            .count(),
        0,
        "the old action must not retry after declaration replacement"
    );

    drop(held_permit);
    clock.advance_by(50);
    tokio::time::advance(Duration::from_secs(5)).await;
    let after = state
        .get_tenant_entity_state(&tenant, "Ticket", "replace-deferred-retry")
        .await
        .expect("observe after releasing old-action capacity");
    assert_eq!(after.state.status, "Closed");
    assert_eq!(
        after
            .state
            .events
            .iter()
            .filter(|event| event.action == "AssignAgent")
            .count(),
        0
    );
}
