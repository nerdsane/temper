//! Deterministic state-timeout declaration hot-swap regressions.

use super::*;
use crate::StorageStack;
use crate::entity_actor::EntityMsg;
use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::state::ServerState;
use crate::state::dispatch::effects::PostDispatchContext;
use std::collections::BTreeMap;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

const TICKET_CSDL: &str = include_str!("../../../../../../test-fixtures/specs/model.csdl.xml");

const TICKET_WITH_OLD_TIMEOUT_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "ExpiredByOld", "ExpiredByNew"]
initial = "Open"
allow_indefinite_states = ["ExpiredByOld", "ExpiredByNew"]

[[action]]
name = "Observe"
kind = "input"
from = ["Open"]
to = "Open"

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
after_seconds = 60
on_timeout = "OldTimeout"
"#;

const TICKET_WITH_CHANGED_TIMEOUT_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "ExpiredByOld", "ExpiredByNew"]
initial = "Open"
allow_indefinite_states = ["ExpiredByOld", "ExpiredByNew"]

[[action]]
name = "Observe"
kind = "input"
from = ["Open"]
to = "Open"

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
after_seconds = 10
on_timeout = "NewTimeout"
"#;

const TICKET_WITH_REMOVED_TIMEOUT_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "ExpiredByOld", "ExpiredByNew"]
initial = "Open"
allow_indefinite_states = ["Open", "ExpiredByOld", "ExpiredByNew"]

[[action]]
name = "Observe"
kind = "input"
from = ["Open"]
to = "Open"

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

async fn setup_hot_swappable_timeout(
    entity_id: &str,
    seed: u64,
    action_dispatch_timeout: Option<Duration>,
) -> (ServerState, TenantId, SimEventStore) {
    let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        TICKET_CSDL.to_string(),
        &[("Ticket", TICKET_WITH_OLD_TIMEOUT_IOA)],
    );
    let sim = SimEventStore::no_faults(seed);
    let mut state = ServerState::from_registry(
        ActorSystem::new(format!("timeout-declaration-hotswap-{entity_id}")),
        registry,
    );
    state.set_storage_stack(StorageStack::from_sim(sim.clone(), None));
    if let Some(timeout) = action_dispatch_timeout {
        state.action_dispatch_timeout = timeout;
    }
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
        actor_uid: None,
    };
    state.arm_state_timeouts_if_needed(&ctx, &created);
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 1)]
    );

    (state, tenant, sim)
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

async fn read_ticket(state: &ServerState, tenant: &TenantId, entity_id: &str) -> EntityResponse {
    state
        .get_tenant_entity_state(tenant, "Ticket", entity_id)
        .await
        .expect("ticket remains readable")
}

#[tokio::test(start_paused = true)]
async fn unchanged_declaration_swap_preserves_the_original_absolute_deadline() {
    let seed = 56;
    let (_guard, clock, _id_gen) = install_deterministic_context(seed);
    let (state, tenant, _sim) =
        setup_hot_swappable_timeout("unchanged-declaration", seed, None).await;

    clock.advance_by(200);
    tokio::time::advance(Duration::from_secs(20)).await;
    hot_swap_ticket_timeout(&state, TICKET_WITH_OLD_TIMEOUT_IOA);
    for _ in 0..32 {
        tokio::task::yield_now().await;
    }

    clock.advance_by(390);
    tokio::time::advance(Duration::from_secs(39)).await;
    assert_eq!(
        read_ticket(&state, &tenant, "unchanged-declaration")
            .await
            .state
            .status,
        "Open",
        "a no-op table version must not fire before the original deadline"
    );

    clock.advance_by(10);
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        read_ticket(&state, &tenant, "unchanged-declaration")
            .await
            .state
            .status,
        "ExpiredByOld",
        "a no-op table version must not restart the sixty-second budget"
    );
}

#[tokio::test(start_paused = true)]
async fn changed_timeout_declaration_rebinds_an_armed_timer() {
    let seed = 53;
    let (_guard, clock, _id_gen) = install_deterministic_context(seed);
    let (state, tenant, _sim) =
        setup_hot_swappable_timeout("changed-declaration", seed, None).await;

    clock.advance_by(50);
    tokio::time::advance(Duration::from_secs(5)).await;
    hot_swap_ticket_timeout(&state, TICKET_WITH_CHANGED_TIMEOUT_IOA);

    clock.advance_by(40);
    tokio::time::advance(Duration::from_secs(4)).await;
    assert_eq!(
        read_ticket(&state, &tenant, "changed-declaration")
            .await
            .state
            .status,
        "Open",
        "the replacement declaration must retain its original durable anchor"
    );

    clock.advance_by(10);
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    let after = read_ticket(&state, &tenant, "changed-declaration").await;
    assert_eq!(after.state.status, "ExpiredByNew");
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
    let (_guard, clock, _id_gen) = install_deterministic_context(seed);
    let (state, tenant, _sim) =
        setup_hot_swappable_timeout("removed-declaration", seed, None).await;
    hot_swap_ticket_timeout(&state, TICKET_WITH_REMOVED_TIMEOUT_IOA);

    clock.advance_by(100);
    tokio::time::advance(Duration::from_secs(10)).await;
    let after = read_ticket(&state, &tenant, "removed-declaration").await;
    assert_eq!(after.state.status, "Open");
    assert_eq!(
        after
            .state
            .events
            .iter()
            .filter(|event| event.action == "OldTimeout")
            .count(),
        0
    );
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 0)]
    );
}

#[tokio::test(start_paused = true)]
async fn transient_state_read_failure_does_not_delay_a_shorter_replacement() {
    let seed = 57;
    let (_guard, clock, _id_gen) = install_deterministic_context(seed);
    let (state, tenant, sim) = setup_hot_swappable_timeout(
        "reconcile-read-failure",
        seed,
        Some(Duration::from_millis(1)),
    )
    .await;
    let entity_id = "reconcile-read-failure";
    let persistence_id = format!("default:Ticket:{entity_id}");
    let actor_ref = state
        .actor_registry
        .read()
        .expect("actor registry lock")
        .get(&persistence_id)
        .expect("timed actor is registered")
        .clone();

    clock.advance_by(50);
    tokio::time::advance(Duration::from_secs(5)).await;
    sim.inject_append_delay(&persistence_id, Duration::from_secs(2));
    actor_ref
        .tell(EntityMsg::Action {
            name: "Observe".to_string(),
            params: serde_json::json!({}),
            cross_entity_booleans: BTreeMap::new(),
            idempotency_key: None,
            state_timeout_precondition: None,
        })
        .expect("enqueue an actor-local action that blocks state reads");
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if sim.pending_append_delays(&persistence_id) == 0 {
            break;
        }
    }
    assert_eq!(sim.pending_append_delays(&persistence_id), 0);

    hot_swap_ticket_timeout(&state, TICKET_WITH_CHANGED_TIMEOUT_IOA);
    for _ in 0..10 {
        clock.advance_by(1);
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
    }
    assert!(
        state.state_timeout_tracker.reconciliation_failure_count() > 0,
        "the blocked actor must exhaust one bounded state-read attempt before recovery"
    );
    for _ in 0..10 {
        clock.advance_by(1);
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
    }
    for _ in 0..10 {
        clock.advance_by(1);
        tokio::time::advance(Duration::from_millis(100)).await;
        tokio::task::yield_now().await;
    }
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }

    let key = EntityKey::new(&tenant, "Ticket", entity_id);
    assert_ne!(
        state.state_timeout_tracker.current_generation(&key),
        1,
        "reconciliation must retry after the one-shot version notification was consumed"
    );
    assert_eq!(
        read_ticket(&state, &tenant, entity_id).await.state.status,
        "Open"
    );

    clock.advance_by(10);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        read_ticket(&state, &tenant, entity_id).await.state.status,
        "Open"
    );

    clock.advance_by(10);
    tokio::time::advance(Duration::from_secs(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
    }
    let after = read_ticket(&state, &tenant, entity_id).await;
    assert_eq!(
        after.state.status, "ExpiredByNew",
        "the shorter replacement must still fire at the original-anchor deadline"
    );
    assert_eq!(
        after
            .state
            .events
            .iter()
            .filter(|event| event.action == "OldTimeout")
            .count(),
        0
    );
}
