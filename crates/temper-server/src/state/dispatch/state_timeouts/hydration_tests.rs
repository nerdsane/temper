//! Focused tests for durable timeout hydration helpers and arm races.

use std::collections::BTreeMap;

use super::*;
use crate::entity_actor::EntityState;
use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::state::ServerState;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::install_deterministic_context;
use temper_spec::csdl::parse_csdl;

const TICKET_CSDL: &str = include_str!("../../../../../../test-fixtures/specs/model.csdl.xml");

const TICKET_WITH_RESET_TIMEOUT_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "InProgress", "Closed"]
initial = "Open"
allow_indefinite_states = ["InProgress", "Closed"]

[[action]]
name = "Reopen"
kind = "input"
from = ["Closed"]
to = "Open"

[[action]]
name = "Heartbeat"
kind = "input"
from = ["Open"]
to = "Open"

[[action]]
name = "Observe"
kind = "input"
from = ["Open"]
to = "Open"

[[action]]
name = "AssignAgent"
kind = "internal"
from = ["Open"]
to = "InProgress"

[[state_timeout]]
state = "Open"
after_seconds = 60
on_timeout = "AssignAgent"
reset_on = ["Heartbeat"]
"#;

fn key() -> EntityKey {
    EntityKey::new(&temper_runtime::tenant::TenantId::new("t"), "E", "1")
}

async fn wait_for_ticket_pending(state: &ServerState, expected: u64) {
    for _ in 0..128 {
        tokio::task::yield_now().await;
        if state.state_timeout_tracker.pending_snapshot() == vec![("Ticket".to_string(), expected)]
        {
            return;
        }
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), expected)]
    );
}

#[tokio::test(start_paused = true)]
async fn readiness_waits_for_the_exact_actor_hydration_barrier() {
    let (_guard, _clock, _ids) = install_deterministic_context(255);
    let tenant = TenantId::default();
    let entity_id = "readiness-hydration-barrier";
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(TICKET_CSDL).expect("CSDL parses"),
        TICKET_CSDL.to_string(),
        &[("Ticket", TICKET_WITH_RESET_TIMEOUT_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new(entity_id), registry);
    let actor = state
        .get_or_spawn_tenant_actor_when_ready(&tenant, "Ticket", entity_id)
        .await
        .expect("the initial actor becomes ready");
    let actor_uid = actor.id().uid;

    // Reinstall the same incarnation's barrier to deterministically model the
    // window after its first-mailbox response but before that response has
    // published timeout ownership. A second GetState must not bypass it.
    let completion = state
        .state_timeout_tracker
        .register_hydration(&tenant, "Ticket", entity_id, actor_uid);
    let mut readiness =
        Box::pin(state.get_or_spawn_tenant_actor_when_ready(&tenant, "Ticket", entity_id));
    tokio::select! {
        biased;
        result = &mut readiness => panic!("readiness bypassed hydration completion: {result:?}"),
        () = tokio::task::yield_now() => {}
    }

    state
        .state_timeout_tracker
        .complete_hydration(&tenant, "Ticket", entity_id, actor_uid, completion);
    let ready = readiness
        .await
        .expect("readiness completes after hydration");
    assert_eq!(ready.id().uid, actor_uid);
}

async fn assert_in_memory_fallback_rearm(action: &str, seed: u64, entity_id: &str) {
    let (_guard, clock, _ids) = install_deterministic_context(seed);
    let tenant = TenantId::default();
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(TICKET_CSDL).expect("CSDL parses"),
        TICKET_CSDL.to_string(),
        &[("Ticket", TICKET_WITH_RESET_TIMEOUT_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new(entity_id), registry);
    let initial = state
        .get_tenant_entity_state(&tenant, "Ticket", entity_id)
        .await
        .expect("initial timed actor starts");
    assert_eq!(
        initial.state.sequence_nr, 0,
        "no event journal is configured"
    );
    assert_eq!(initial.state.total_event_count, 0);
    assert_eq!(initial.state.state_timeout_clock_reset_at, None);
    assert_eq!(initial.state.state_timeout_clock_reset_version, None);
    wait_for_ticket_pending(&state, 1).await;

    tokio::time::advance(Duration::from_secs(20)).await;
    clock.advance_by(200);
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Ticket",
            entity_id,
            action,
            serde_json::json!({}),
            &AgentContext::for_service("timeout-scheduler-test"),
        )
        .await
        .expect("same-state action applies");
    assert!(response.success);
    assert_eq!(response.state.sequence_nr, 0);
    assert_eq!(response.state.total_event_count, 1);
    assert_eq!(response.state.state_timeout_clock_reset_version, Some(1));
    wait_for_ticket_pending(&state, 1).await;

    tokio::time::advance(Duration::from_secs(40)).await;
    clock.advance_by(400);
    wait_for_ticket_pending(&state, 1).await;
    let before_new_deadline = state
        .get_tenant_entity_state(&tenant, "Ticket", entity_id)
        .await
        .expect("entity remains readable at the superseded deadline");
    assert_eq!(
        before_new_deadline.state.status, "Open",
        "the hydrated fallback deadline must not fire after the clock is established"
    );

    tokio::time::advance(Duration::from_secs(20)).await;
    clock.advance_by(200);
    wait_for_ticket_pending(&state, 0).await;
    let after_new_deadline = state
        .get_tenant_entity_state(&tenant, "Ticket", entity_id)
        .await
        .expect("entity remains readable after timeout");
    assert_eq!(after_new_deadline.state.status, "InProgress");
}

#[tokio::test(start_paused = true)]
async fn in_memory_reset_replaces_the_hydrated_fallback_deadline() {
    assert_in_memory_fallback_rearm("Heartbeat", 217, "in-memory-reset").await;
}

#[tokio::test(start_paused = true)]
async fn first_unrelated_event_reconciles_the_missing_in_memory_anchor() {
    assert_in_memory_fallback_rearm("Observe", 218, "in-memory-anchor-repair").await;
}

#[test]
fn dispatch_arm_wins_when_it_precedes_hydration_reconciliation() {
    let tracker = StateTimeoutTracker::new();
    let entity = key();

    let dispatch_seq = tracker
        .advance_if_fresh(&entity, 2, None, None, None)
        .expect("new dispatch claims timeout ownership")
        .generation;
    assert!(
        tracker
            .reconcile_if_fresh(&entity, 1, None, None, None)
            .is_none()
    );
    assert_eq!(
        tracker.current_generation(&entity),
        dispatch_seq,
        "late hydration must not invalidate the live dispatch deadline"
    );
}

#[test]
fn dispatch_arm_supersedes_an_earlier_hydration_reservation() {
    let tracker = StateTimeoutTracker::new();
    let entity = key();

    let hydration_seq = tracker
        .reconcile_if_fresh(&entity, 1, None, None, None)
        .expect("hydration claims an unarmed entity")
        .generation;
    let dispatch_seq = tracker
        .advance_if_fresh(&entity, 2, None, None, None)
        .expect("newer dispatch supersedes hydration")
        .generation;
    assert_ne!(hydration_seq, dispatch_seq);
    assert_eq!(
        tracker.current_generation(&entity),
        dispatch_seq,
        "a real transition must retain the only current deadline"
    );
}

#[test]
fn hydration_delay_seed_sweep_covers_remaining_exact_and_overdue_budgets() {
    let budget = Duration::from_secs(60);
    let entry = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
    let mut events = VecDeque::new();
    events.push_back(EntityEvent {
        action: "Start".to_string(),
        from_status: "Idle".to_string(),
        to_status: "Running".to_string(),
        timestamp: entry,
        params: serde_json::json!({}),
        idempotency_key: None,
    });

    let mut saw_remaining = false;
    let mut saw_exact = false;
    let mut saw_overdue = false;
    for seed in 0_u64..128 {
        let elapsed_secs = seed.wrapping_mul(37) % 121;
        let now = entry + chrono::Duration::seconds(elapsed_secs as i64);
        let hydration = compute_timeout_delay(&events, None, "Running", &[], budget, now).unwrap();
        assert_eq!(
            hydration.delay,
            budget.saturating_sub(Duration::from_secs(elapsed_secs)),
            "seed {seed} must recover the exact remaining budget"
        );
        assert_eq!(hydration.overdue, elapsed_secs >= 60);
        saw_remaining |= elapsed_secs < 60;
        saw_exact |= elapsed_secs == 60;
        saw_overdue |= elapsed_secs > 60;
    }

    assert!(saw_remaining, "seed sweep must cover a remaining budget");
    assert!(saw_exact, "seed sweep must cover the exact deadline");
    assert!(saw_overdue, "seed sweep must cover overdue recovery");
}

#[test]
fn reconciliation_charges_only_time_after_a_later_durable_entry() {
    let observed_at = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
    let entered_at = observed_at + chrono::Duration::seconds(3);
    let events = VecDeque::from([EntityEvent {
        action: "Created".to_string(),
        from_status: String::new(),
        to_status: "Running".to_string(),
        timestamp: entered_at,
        params: serde_json::json!({}),
        idempotency_key: None,
    }]);
    let reconciled_at = hydration_reconciled_at(observed_at, Duration::from_secs(5));

    assert_eq!(
        compute_timeout_delay(
            &events,
            Some(entered_at),
            "Running",
            &[],
            Duration::from_secs(60),
            reconciled_at,
        ),
        Some(TimeoutDelay {
            delay: Duration::from_secs(58),
            overdue: false,
        }),
        "readiness before the durable Created event must not consume its timeout budget"
    );
}

#[tokio::test(start_paused = true)]
async fn delayed_post_dispatch_entry_and_reset_keep_the_durable_deadline() {
    let (_guard, _clock, _ids) = install_deterministic_context(214);
    let tenant = TenantId::default();
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(TICKET_CSDL).expect("CSDL parses"),
        TICKET_CSDL.to_string(),
        &[("Ticket", TICKET_WITH_RESET_TIMEOUT_IOA)],
    );
    let state =
        ServerState::from_registry(ActorSystem::new("post-dispatch-durable-deadline"), registry);
    let agent_ctx = AgentContext::for_service("timeout-scheduler-test");
    let action_params = serde_json::json!({});
    let durable_anchor = sim_now() - chrono::Duration::seconds(20);

    let response = |entity_id: &str, action: &str, from_status: &str| EntityResponse {
        success: true,
        state: EntityState {
            entity_type: "Ticket".to_string(),
            entity_id: entity_id.to_string(),
            status: "Open".to_string(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields: serde_json::json!({"Id": entity_id, "Status": "Open"}),
            events: VecDeque::from([EntityEvent {
                action: action.to_string(),
                from_status: from_status.to_string(),
                to_status: "Open".to_string(),
                timestamp: durable_anchor,
                params: serde_json::json!({}),
                idempotency_key: None,
            }]),
            state_timeout_clock_reset_at: Some(durable_anchor),
            state_timeout_clock_reset_version: Some(1),
            total_event_count: 1,
            events_since_snapshot: 1,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 1,
            processed_idempotency_keys: BTreeMap::new(),
        },
        error: None,
        custom_effects: Vec::new(),
        scheduled_actions: Vec::new(),
        spawn_requests: Vec::new(),
        spec_governed: true,
    };

    for (entity_id, action, from_status) in [
        ("delayed-entry", "Reopen", "Closed"),
        ("delayed-reset", "Heartbeat", "Open"),
    ] {
        let ctx = PostDispatchContext {
            tenant: &tenant,
            entity_type: "Ticket",
            entity_id,
            action,
            agent_ctx: &agent_ctx,
            dispatch_idempotency_key: None,
            action_params: &action_params,
            await_integration: false,
            actor_uid: None,
        };
        state.arm_state_timeouts_if_needed(&ctx, &response(entity_id, action, from_status));
    }

    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 2)],
        "both delayed post-dispatch paths must arm one timer"
    );

    // The durable entry/reset happened 20 seconds before post-dispatch
    // arming, so only 40 seconds remain from the declared 60-second budget.
    tokio::time::advance(Duration::from_millis(39_999)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 2)],
        "neither deadline may fire early"
    );

    tokio::time::advance(Duration::from_millis(1)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if state.state_timeout_tracker.pending_snapshot() == vec![("Ticket".to_string(), 0)] {
            break;
        }
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 0)],
        "entry and reset timers must consume post-event persistence/effect time instead of granting a fresh full budget"
    );
}

#[tokio::test(start_paused = true)]
async fn reverse_ordered_reset_callbacks_keep_the_newest_durable_deadline() {
    let (_guard, _clock, _ids) = install_deterministic_context(215);
    let tenant = TenantId::default();
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        tenant.as_str(),
        parse_csdl(TICKET_CSDL).expect("CSDL parses"),
        TICKET_CSDL.to_string(),
        &[("Ticket", TICKET_WITH_RESET_TIMEOUT_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("reverse-reset-deadline"), registry);
    let agent_ctx = AgentContext::for_service("timeout-scheduler-test");
    let action_params = serde_json::json!({});
    let now = sim_now();
    let older_anchor = now - chrono::Duration::seconds(20);
    let newer_anchor = now - chrono::Duration::seconds(10);

    let response = |sequence_nr: u64, durable_anchor: DateTime<Utc>| EntityResponse {
        success: true,
        state: EntityState {
            entity_type: "Ticket".to_string(),
            entity_id: "reverse-reset".to_string(),
            status: "Open".to_string(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields: serde_json::json!({"Id": "reverse-reset", "Status": "Open"}),
            events: VecDeque::from([EntityEvent {
                action: "Heartbeat".to_string(),
                from_status: "Open".to_string(),
                to_status: "Open".to_string(),
                timestamp: durable_anchor,
                params: serde_json::json!({}),
                idempotency_key: None,
            }]),
            state_timeout_clock_reset_at: Some(durable_anchor),
            state_timeout_clock_reset_version: Some(sequence_nr),
            total_event_count: sequence_nr as usize,
            events_since_snapshot: 1,
            last_snapshot_sequence_nr: sequence_nr - 1,
            sequence_nr,
            processed_idempotency_keys: BTreeMap::new(),
        },
        error: None,
        custom_effects: Vec::new(),
        scheduled_actions: Vec::new(),
        spawn_requests: Vec::new(),
        spec_governed: true,
    };
    let ctx = PostDispatchContext {
        tenant: &tenant,
        entity_type: "Ticket",
        entity_id: "reverse-reset",
        action: "Heartbeat",
        agent_ctx: &agent_ctx,
        dispatch_idempotency_key: None,
        action_params: &action_params,
        await_integration: false,
        actor_uid: None,
    };

    // Both transitions committed in actor order, but the newer response's
    // post-dispatch effects completed first. The late older callback must not
    // supersede the already-armed durable deadline.
    state.arm_state_timeouts_if_needed(&ctx, &response(11, newer_anchor));
    state.arm_state_timeouts_if_needed(&ctx, &response(10, older_anchor));

    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 1)],
        "the stale reset callback must be rejected instead of arming a second timer"
    );

    tokio::time::advance(Duration::from_secs(40)).await;
    tokio::task::yield_now().await;
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 1)],
        "the older durable anchor must not fire ten seconds before the newest reset deadline"
    );

    tokio::time::advance(Duration::from_secs(10)).await;
    for _ in 0..64 {
        tokio::task::yield_now().await;
        if state.state_timeout_tracker.pending_snapshot() == vec![("Ticket".to_string(), 0)] {
            break;
        }
    }
    assert_eq!(
        state.state_timeout_tracker.pending_snapshot(),
        vec![("Ticket".to_string(), 0)],
        "the single accepted timer must complete at the newest durable reset deadline"
    );
}

#[path = "hydration_deadline_tests.rs"]
mod deadline_tests;
