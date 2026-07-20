//! Core timeout ownership and load regressions.

use std::collections::BTreeMap;

use super::*;
use temper_runtime::tenant::TenantId;

fn key() -> EntityKey {
    EntityKey::new(&TenantId::from("t".to_string()), "E", "e-1")
}

#[test]
fn fresh_durable_responses_advance_monotonic_generations() {
    let t = StateTimeoutTracker::new();
    let k = key();
    assert_eq!(t.current_generation(&k), 0, "initial generation is 0");
    assert_eq!(
        t.advance_if_fresh(&k, 7, None, None, None)
            .map(|permit| permit.generation),
        Some(1)
    );
    assert_eq!(
        t.advance_if_fresh(&k, 8, None, None, None)
            .map(|permit| permit.generation),
        Some(2)
    );
    assert_eq!(
        t.advance_if_fresh(&k, 9, None, None, None)
            .map(|permit| permit.generation),
        Some(3)
    );
    assert_eq!(t.current_generation(&k), 3);
}

#[test]
fn stale_or_duplicate_durable_responses_cannot_advance_ownership() {
    let t = StateTimeoutTracker::new();
    let k = key();
    assert_eq!(
        t.advance_if_fresh(&k, 11, None, None, None)
            .map(|permit| permit.generation),
        Some(1)
    );
    assert!(t.advance_if_fresh(&k, 10, None, None, None).is_none());
    assert!(t.advance_if_fresh(&k, 11, None, None, None).is_none());
    assert_eq!(t.current_generation(&k), 1);
}

#[test]
fn per_entity_owners_are_independent() {
    let t = StateTimeoutTracker::new();
    let a = EntityKey::new(&TenantId::from("t".to_string()), "E", "a");
    let b = EntityKey::new(&TenantId::from("t".to_string()), "E", "b");
    assert_eq!(
        t.advance_if_fresh(&a, 1, None, None, None)
            .map(|permit| permit.generation),
        Some(1)
    );
    assert_eq!(
        t.advance_if_fresh(&a, 2, None, None, None)
            .map(|permit| permit.generation),
        Some(2)
    );
    assert_eq!(
        t.advance_if_fresh(&b, 1, None, None, None)
            .map(|permit| permit.generation),
        Some(3),
        "b's owner is independent of a's"
    );
    assert_eq!(t.current_generation(&a), 2);
    assert_eq!(t.current_generation(&b), 3);
}

#[test]
fn forget_releases_entity() {
    let t = StateTimeoutTracker::new();
    let tenant = TenantId::from("t".to_string());
    let key = EntityKey::new(&tenant, "E", "x");
    let permit = t
        .advance_if_fresh(&key, 1, None, None, None)
        .expect("first owner is accepted");
    assert_eq!(permit.generation, 1);
    assert!(!*permit.cancellation.borrow());
    assert_eq!(t.size(), 1);
    t.forget(&tenant, "E", "x");
    assert!(*permit.cancellation.borrow());
    assert_eq!(t.size(), 0);
    let replacement = t
        .advance_if_fresh(&key, 1, None, None, None)
        .expect("replacement owner is accepted");
    assert!(
        replacement.generation > permit.generation,
        "owner generations must not ABA after reclamation"
    );
}

#[test]
fn inactive_fence_is_created_without_a_prior_owner() {
    let tracker = StateTimeoutTracker::new();
    let key = key();
    let fence = tracker
        .fence_inactive_if_fresh(&key, 7, None, None)
        .expect("synthetic exit establishes an absent-owner fence");

    assert_eq!(tracker.current_generation(&key), fence.generation);
    assert!(tracker.forget_inactive_if_current(&key, fence));
    assert_eq!(tracker.current_generation(&key), 0);
}

#[test]
fn stale_eviction_cannot_remove_a_newer_inactive_fence() {
    let tracker = StateTimeoutTracker::new();
    let key = key();
    let stale = tracker
        .fence_inactive_if_fresh(&key, 7, None, None)
        .expect("first synthetic fence");
    let current = tracker
        .fence_inactive_if_fresh(&key, 8, None, None)
        .expect("newer synthetic fence");

    assert!(
        !tracker.forget_inactive_if_current(&key, stale),
        "cleanup must compare exact fence provenance"
    );
    assert_eq!(tracker.current_generation(&key), current.generation);
    assert!(tracker.forget_inactive_if_current(&key, current));
    assert_eq!(tracker.current_generation(&key), 0);
}

#[test]
fn invalidation_signals_the_armed_task_before_retaining_order() {
    let t = StateTimeoutTracker::new();
    let key = key();
    let declaration = StateTimeout {
        state: "Open".to_string(),
        after_seconds: u64::MAX,
        on_timeout: "TimeoutFail".to_string(),
        max_occurrences: 1,
        reset_on: Vec::new(),
        params: BTreeMap::new(),
    };
    let permit = t
        .advance_if_fresh(&key, 1, None, None, Some(&declaration))
        .expect("timed owner is accepted");

    assert!(t.invalidate_if_fresh(&key, 2, None, None));
    assert!(
        *permit.cancellation.borrow(),
        "invalidation must wake an arbitrarily distant timer"
    );
    assert_eq!(
        t.size(),
        1,
        "the monotonic response order remains until actor eviction"
    );
}

#[test]
fn first_untimed_response_fences_delayed_older_hydration() {
    let tracker = StateTimeoutTracker::new();
    let key = key();
    let declaration = StateTimeout {
        state: "Open".to_string(),
        after_seconds: u64::MAX,
        on_timeout: "TimeoutFail".to_string(),
        max_occurrences: 1,
        reset_on: Vec::new(),
        params: BTreeMap::new(),
    };

    assert!(tracker.invalidate_if_fresh(&key, 2, None, None));
    assert_eq!(tracker.size(), 1, "the untimed high-water mark is retained");
    assert!(
        tracker
            .reconcile_if_fresh(&key, 1, None, None, Some(&declaration))
            .is_none(),
        "older hydration must not install a timer after an untimed response"
    );
}

#[test]
fn newer_untimed_response_advances_an_existing_inactive_high_water() {
    let tracker = StateTimeoutTracker::new();
    let key = key();
    let declaration = StateTimeout {
        state: "Open".to_string(),
        after_seconds: u64::MAX,
        on_timeout: "TimeoutFail".to_string(),
        max_occurrences: 1,
        reset_on: Vec::new(),
        params: BTreeMap::new(),
    };

    assert!(tracker.invalidate_if_fresh(&key, 3, None, None));
    let generation_at_three = tracker.current_generation(&key);
    assert!(tracker.invalidate_if_fresh(&key, 4, None, None));
    assert!(tracker.current_generation(&key) > generation_at_three);
    assert!(
        tracker
            .reconcile_if_fresh(&key, 3, None, None, Some(&declaration))
            .is_none(),
        "a delayed timed callback cannot overtake the newer untimed exit"
    );
}

// --- compute_state_clock_reset_ts (ADR-0056 hydration-re-arm helper) ---

fn test_event(action: &str, from: &str, to: &str, ts_ms_after_epoch: i64) -> EntityEvent {
    let ts = DateTime::<Utc>::from_timestamp_millis(ts_ms_after_epoch).unwrap();
    EntityEvent {
        action: action.to_string(),
        from_status: from.to_string(),
        to_status: to.to_string(),
        timestamp: ts,
        params: serde_json::json!({}),
        idempotency_key: None,
    }
}

#[test]
fn clock_reset_finds_most_recent_entry_event() {
    let mut events = VecDeque::new();
    events.push_back(test_event("Create", "", "Open", 1_000));
    events.push_back(test_event("Assign", "Open", "InProgress", 2_000));
    events.push_back(test_event("Close", "InProgress", "Closed", 3_000));

    // Current state Closed → clock reset == Close event timestamp.
    let reset = compute_state_clock_reset_ts(&events, None, "Closed", &[]).unwrap();
    assert_eq!(reset.timestamp_millis(), 3_000);
}

#[test]
fn clock_reset_prefers_reset_on_event_after_entry() {
    let mut events = VecDeque::new();
    events.push_back(test_event("Enter", "", "Executing", 100));
    events.push_back(test_event("DoWork", "Executing", "Executing", 500));
    events.push_back(test_event("ProgressMade", "Executing", "Executing", 900));
    events.push_back(test_event("OtherAction", "Executing", "Executing", 1_200));

    let reset_on = vec!["ProgressMade".to_string()];
    let reset = compute_state_clock_reset_ts(&events, None, "Executing", &reset_on).unwrap();
    assert_eq!(
        reset.timestamp_millis(),
        900,
        "latest reset_on event wins over later non-reset events"
    );
}

#[test]
fn clock_reset_falls_back_to_entry_when_no_reset_events() {
    let mut events = VecDeque::new();
    events.push_back(test_event("Configure", "Queued", "Ready", 500));
    events.push_back(test_event("Start", "Ready", "Executing", 1_000));
    events.push_back(test_event("Steer", "Executing", "Executing", 1_500));

    let reset_on = vec!["ProgressMade".to_string()];
    let reset = compute_state_clock_reset_ts(&events, None, "Executing", &reset_on).unwrap();
    assert_eq!(
        reset.timestamp_millis(),
        1_000,
        "Steer is not a reset_on; entry timestamp wins"
    );
}

#[test]
fn clock_reset_returns_none_when_no_entry_event_retained() {
    let mut events = VecDeque::new();
    // Only self-loops retained in the window; the original transition
    // into `Executing` has been snapshotted and forgotten.
    events.push_back(test_event("Steer", "Executing", "Executing", 100));
    events.push_back(test_event("Steer", "Executing", "Executing", 200));

    let reset = compute_state_clock_reset_ts(&events, None, "Executing", &[]);
    assert!(reset.is_none(), "no entry event in window → None");
}

#[test]
fn clock_reset_ignores_entry_events_for_other_states() {
    let mut events = VecDeque::new();
    events.push_back(test_event("Create", "", "Open", 1_000));
    events.push_back(test_event("Assign", "Open", "InProgress", 2_000));
    // Query for Open, but the current state is InProgress — no match.
    let reset = compute_state_clock_reset_ts(&events, None, "Open", &[]);
    // The events.back() is InProgress, so no entry-into-Open event
    // with from != to is in the window; the original entry at index 0
    // has from_status="" which satisfies "!= current_state", so it IS
    // considered an entry-into-Open event — clock reset == 1_000.
    assert_eq!(reset.unwrap().timestamp_millis(), 1_000);
}

#[test]
fn clock_reset_ignores_self_loop_events_as_entry() {
    // Self-loops have from == to, so they must NOT be treated as entry.
    // The prior transition is the true entry point.
    let mut events = VecDeque::new();
    events.push_back(test_event("Create", "", "Executing", 100));
    events.push_back(test_event("Steer", "Executing", "Executing", 500));
    events.push_back(test_event("Steer", "Executing", "Executing", 800));

    let reset = compute_state_clock_reset_ts(&events, None, "Executing", &[]).unwrap();
    assert_eq!(
        reset.timestamp_millis(),
        100,
        "first real entry wins; subsequent self-loops don't re-enter"
    );
}

// ------------------------------------------------------------------
// Integration test: prove the runtime scheduler actually fires.
// ------------------------------------------------------------------

use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::state::dispatch::effects::PostDispatchContext;
use crate::state::{DispatchCommand, ServerState};
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;

const TICKET_CSDL: &str = include_str!("../../../../../../test-fixtures/specs/model.csdl.xml");

/// Custom Ticket IOA with a state_timeout on `Open`. Fires `AssignAgent`
/// after 1 second; the action transitions the ticket to `InProgress`.
/// By default `AssignAgent` is an input action from `Open`, so the
/// auto-wiring stays idempotent.
const TICKET_WITH_TIMEOUT_IOA: &str = r#"
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
"#;

/// Ticket spec with tight admission caps — used to prove admission
/// control actually gates concurrent dispatches end-to-end.
const TICKET_WITH_ADMISSION_IOA: &str = r#"
[automaton]
name = "Ticket"
states = ["Open", "InProgress", "WaitingOnCustomer", "Resolved", "Closed"]
initial = "Open"
allow_indefinite_states = ["Open", "InProgress", "WaitingOnCustomer", "Resolved", "Closed"]

[[state]]
name = "replies"
type = "counter"
initial = "0"

[[action]]
name = "AssignAgent"
kind = "input"
from = ["Open"]
to = "InProgress"

[admission]
max_concurrent_creates = 5
max_concurrent_actions = { "AssignAgent" = 5 }
queue_depth = 1000
queue_timeout_seconds = 10
"#;

// Incident-replay style load proof: 120 concurrent dispatches against an
// admission cap of 5. With the pre-fix behavior (no admission + fixed 5s
// ask timeout + no retry), a subset would surface as HTTP 500. With the
// fix in place, every caller either (a) is granted and succeeds, or (b)
// gets Deferred (503 Retry-After) — no 500s, no mailbox-full drops, and
// the cap is strictly enforced (never more than 5 in flight).
//
// Also reports throughput and latency percentiles so the performance
// baseline is tracked alongside correctness.

#[path = "core_tests/admission.rs"]
mod admission;
#[path = "core_tests/deferral.rs"]
mod deferral;
#[path = "core_tests/fire.rs"]
mod fire;
#[path = "core_tests/throughput.rs"]
mod throughput;
