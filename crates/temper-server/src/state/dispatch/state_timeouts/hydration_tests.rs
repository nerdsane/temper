//! Focused tests for durable timeout hydration helpers and arm races.

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
    EntityKey {
        tenant: "t".into(),
        entity_type: "E".into(),
        entity_id: "1".into(),
    }
}

#[test]
fn dispatch_arm_wins_when_it_precedes_hydration_reconciliation() {
    let tracker = StateTimeoutTracker::new();
    let entity = key();

    let dispatch_seq = tracker.bump(&entity);
    assert_eq!(tracker.reserve_if_unarmed(&entity), None);
    assert_eq!(
        tracker.current(&entity),
        dispatch_seq,
        "late hydration must not invalidate the live dispatch deadline"
    );
}

#[test]
fn dispatch_arm_supersedes_an_earlier_hydration_reservation() {
    let tracker = StateTimeoutTracker::new();
    let entity = key();

    let hydration_seq = tracker
        .reserve_if_unarmed(&entity)
        .expect("hydration claims an unarmed entity");
    let dispatch_seq = tracker.bump(&entity);
    assert_ne!(hydration_seq, dispatch_seq);
    assert_eq!(
        tracker.current(&entity),
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
        let hydration =
            compute_hydration_delay(&events, None, "Running", &[], budget, now).unwrap();
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
        compute_hydration_delay(
            &events,
            Some(entered_at),
            "Running",
            &[],
            Duration::from_secs(60),
            reconciled_at,
        ),
        Some(HydrationDelay {
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

#[test]
fn snapshot_anchor_survives_an_empty_recent_event_window() {
    let reset_at = DateTime::<Utc>::from_timestamp(1_700_000_000, 0).unwrap();
    assert_eq!(
        compute_state_clock_reset_ts(&VecDeque::new(), Some(reset_at), "Running", &[]),
        Some(reset_at),
        "a current snapshot must retain the durable timeout anchor"
    );
}

#[tokio::test(start_paused = true)]
async fn absolute_deadline_survives_timer_task_poll_delay() {
    let deadline = timeout_deadline(Duration::from_secs(10));

    // Model a spawned timer task that receives no CPU for four seconds.
    tokio::time::advance(Duration::from_secs(4)).await;
    let timer = tokio::spawn(async move { tokio::time::sleep_until(deadline).await });
    tokio::task::yield_now().await;

    tokio::time::advance(Duration::from_millis(5_999)).await;
    tokio::task::yield_now().await;
    assert!(
        !timer.is_finished(),
        "the timer must not fire before its deadline"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    timer
        .await
        .expect("task queue time must not move the precomputed deadline later");
}
