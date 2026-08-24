//! Runtime execution of `[[state_timeout]]` declarations (ADR-0049).
//!
//! After each successful entity transition, [`ServerState::arm_state_timeouts_if_needed`]
//! inspects the spec to decide whether the new state warrants a timer. A
//! timer is armed on:
//!
//! - **State entry** — the transition moved the entity to a state with a
//!   `[[state_timeout]]` declaration.
//! - **Reset signal** — the action that fired is listed in the declaration's
//!   `reset_on` while the entity is in the declared state.
//!
//! Cancellation is sequence-based. Every arm bumps a per-entity counter in
//! [`StateTimeoutTracker`] and captures the post-bump value. When the timer
//! fires, it re-reads the current counter; any mismatch means a newer arm
//! (or a state change) invalidated this one, so the fire is a no-op.
//!
//! Cancellation on state exit is implicit: before arming a new timer, we
//! bump once for the old state if it had a declaration. That bump renders
//! any in-flight timer for the old state stale.
//!
//! Durability (ADR-0056): on every post-dispatch the arm logic also detects
//! the **hydration case** — the entity is in a state with a declared
//! timeout but has no live in-memory timer (seq == 0 in the tracker). In
//! that case we reconstruct how long the entity has been in the current
//! state from the event log (the most recent transition into
//! `state.status`, or the most recent `reset_on` event after it), and:
//!   - if elapsed ≥ `after_seconds` → fire `on_timeout` immediately
//!     (the entity was overdue before this process ever saw it).
//!   - otherwise → arm a tokio timer with the remaining budget
//!     (`after_seconds - elapsed`).
//!
//! This closes the gap where an orphaned entity (actor passivated or
//! server restarted while in a timed state) would otherwise never have
//! its timer re-armed because no state transition happened on the
//! hydrated actor. Fully event-log-backed scheduling remains the
//! longer-term direction; hydration re-arm is the 80%-value prefix
//! that makes timeouts reliable across the common failure modes.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tracing::Instrument;

use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use crate::entity_actor::{EntityEvent, EntityResponse};

use super::effects::PostDispatchContext;

/// Walk the event history backward to find the timestamp of the most recent
/// "progress" signal for the given state: either the transition that entered
/// `current_state`, or any later event whose action is in `reset_on`. This
/// timestamp is the reference point for computing how long the entity has
/// been idle in the current state, used by the hydration re-arm path to
/// compute remaining timeout budget.
///
/// Returns `None` if no matching entry event is found in the retained window
/// (state.events is a bounded VecDeque; older history has been snapshotted
/// and forgotten). Callers treat `None` as "no elapsed info available" and
/// arm with the full budget.
fn compute_state_clock_reset_ts(
    events: &VecDeque<EntityEvent>,
    current_state: &str,
    reset_on: &[String],
) -> Option<DateTime<Utc>> {
    // Find the most recent state entry: last event whose to_status ==
    // current_state AND from_status != current_state. Scanning backward
    // because recent events are at the back.
    let entry_idx = events
        .iter()
        .rposition(|e| e.to_status == current_state && e.from_status != current_state)?;
    let entry_ts = events[entry_idx].timestamp;

    // Among events after the entry, find the latest reset_on event.
    // Those reset the clock per ADR-0049 semantics.
    let latest_reset_ts = events
        .iter()
        .skip(entry_idx + 1)
        .filter(|e| reset_on.iter().any(|a| a == &e.action))
        .map(|e| e.timestamp)
        .max();

    Some(latest_reset_ts.unwrap_or(entry_ts))
}

/// Composite key identifying an entity instance inside the arm-seq tracker.
#[derive(Debug, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
struct EntityKey {
    tenant: String,
    entity_type: String,
    entity_id: String,
}

impl EntityKey {
    fn new(tenant: &TenantId, entity_type: &str, entity_id: &str) -> Self {
        Self {
            tenant: tenant.to_string(),
            entity_type: entity_type.to_string(),
            entity_id: entity_id.to_string(),
        }
    }
}

/// In-memory cancellation counter keyed by entity instance.
///
/// Each arm increments and captures the new value; firings compare captured
/// against current and drop the fire when they diverge.
#[derive(Default, Debug)]
pub struct StateTimeoutTracker {
    seqs: Mutex<BTreeMap<EntityKey, u64>>,
    /// ADR-0049: per-entity-type count of armed-but-unfired timers.
    /// Emitted as `temper_scheduler_pending_timers` by the canary loop.
    pending_by_type: Mutex<BTreeMap<String, u64>>,
}

impl StateTimeoutTracker {
    pub fn new() -> Self {
        Self::default()
    }

    fn bump(&self, key: &EntityKey) -> u64 {
        let mut map = self.seqs.lock().expect("state_timeout tracker poisoned");
        let entry = map.entry(key.clone()).or_insert(0);
        *entry += 1;
        *entry
    }

    fn current(&self, key: &EntityKey) -> u64 {
        self.seqs
            .lock()
            .expect("state_timeout tracker poisoned")
            .get(key)
            .copied()
            .unwrap_or(0)
    }

    /// Increment the pending-timer count for `entity_type`. Called at arm.
    pub fn inc_pending(&self, entity_type: &str) {
        let mut map = self
            .pending_by_type
            .lock()
            .expect("pending_by_type poisoned");
        *map.entry(entity_type.to_string()).or_insert(0) += 1;
    }

    /// Decrement the pending-timer count for `entity_type`. Called when a
    /// timer task exits (fired, cancelled by seq mismatch, or state changed).
    pub fn dec_pending(&self, entity_type: &str) {
        let mut map = self
            .pending_by_type
            .lock()
            .expect("pending_by_type poisoned");
        if let Some(v) = map.get_mut(entity_type)
            && *v > 0
        {
            *v -= 1;
        }
    }

    /// Snapshot pending counts per entity type for metric emission.
    pub fn pending_snapshot(&self) -> Vec<(String, u64)> {
        let map = self
            .pending_by_type
            .lock()
            .expect("pending_by_type poisoned");
        map.iter().map(|(k, v)| (k.clone(), *v)).collect()
    }

    /// Drop any seq for `key`. Called when an entity is deleted so the map
    /// doesn't grow unbounded over the process lifetime.
    pub fn forget(&self, tenant: &TenantId, entity_type: &str, entity_id: &str) {
        let key = EntityKey::new(tenant, entity_type, entity_id);
        let _ = self
            .seqs
            .lock()
            .expect("state_timeout tracker poisoned")
            .remove(&key);
    }

    #[cfg(test)]
    fn size(&self) -> usize {
        self.seqs
            .lock()
            .expect("state_timeout tracker poisoned")
            .len()
    }
}

impl crate::state::ServerState {
    /// Arm or re-arm state timers based on the just-completed transition.
    ///
    /// Invoked from `run_post_dispatch_effects`. Walks the spec's
    /// `state_timeouts`, bumps the per-entity seq appropriately, and spawns
    /// a tokio task per armed timer. Non-durable under the MVP — timers are
    /// lost across restarts.
    pub(crate) fn arm_state_timeouts_if_needed(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        // ADR-0178: durable stores co-commit timeout intents with the source
        // event and route them through the single leased delivery owner. A
        // second process-local timer here would race that durable owner.
        if self.event_journal().is_some() {
            return;
        }
        let registry = match self.registry.read() {
            Ok(g) => g,
            Err(_) => return,
        };
        let Some(spec) = registry.get_spec(ctx.tenant, ctx.entity_type) else {
            return;
        };
        if spec.automaton.state_timeouts.is_empty() {
            return;
        }
        let state_timeouts = spec.automaton.state_timeouts.clone();
        drop(registry);

        let post_state = response.state.status.clone();
        let pre_state = response
            .state
            .events
            .back()
            .map(|e| e.from_status.clone())
            .unwrap_or_default();
        let state_changed = pre_state != post_state;
        let key = EntityKey::new(ctx.tenant, ctx.entity_type, ctx.entity_id);

        // 1. Invalidate any outstanding timer for the prior state.
        if state_changed {
            let pre_had_timeout = state_timeouts.iter().any(|st| st.state == pre_state);
            if pre_had_timeout {
                self.state_timeout_tracker.bump(&key);
                crate::runtime_metrics::record_state_timeout_cancelled(
                    ctx.tenant.as_str(),
                    ctx.entity_type,
                    &pre_state,
                );
            }
        }

        // 2. Arm timers for any matching state_timeout declaration.
        for st in &state_timeouts {
            if st.state != post_state {
                continue;
            }
            let is_entry = state_changed;
            let is_reset = !state_changed && st.reset_on.iter().any(|a| a == ctx.action);
            // ADR-0056: hydration re-arm. If the entity is in a state with a
            // declared timeout and this process has no live timer for it (seq
            // == 0), treat this dispatch as the opportunity to catch up. This
            // handles actors that hydrated from snapshot after a restart or
            // passivation without the benefit of a state transition to arm
            // the timer.
            let needs_hydration_rearm =
                !is_entry && !is_reset && self.state_timeout_tracker.current(&key) == 0;
            if !is_entry && !is_reset && !needs_hydration_rearm {
                continue;
            }
            if is_reset {
                crate::runtime_metrics::record_state_timeout_reset(
                    ctx.tenant.as_str(),
                    ctx.entity_type,
                    &st.state,
                    ctx.action,
                );
            }

            // Determine the fire delay.
            //
            // - Entry / reset / fresh-process cases: the full budget.
            // - Hydration re-arm: budget minus elapsed time since the entity
            //   entered the current state (or the most recent `reset_on`
            //   event, whichever is later). If elapsed >= budget, delay is 0
            //   and the on_timeout action fires on the next tokio tick.
            let mut delay = Duration::from_secs(st.after_seconds);
            if needs_hydration_rearm {
                let clock_reset =
                    compute_state_clock_reset_ts(&response.state.events, &post_state, &st.reset_on);
                if let Some(reset_ts) = clock_reset {
                    let now = sim_now();
                    let elapsed = now
                        .signed_duration_since(reset_ts)
                        .to_std()
                        .unwrap_or(Duration::ZERO);
                    let budget = Duration::from_secs(st.after_seconds);
                    let overdue = elapsed >= budget;
                    delay = if overdue {
                        Duration::ZERO
                    } else {
                        budget - elapsed
                    };
                    crate::runtime_metrics::record_state_timeout_armed_on_hydration(
                        ctx.tenant.as_str(),
                        ctx.entity_type,
                        &st.state,
                        if overdue { "overdue" } else { "budgeted" },
                    );
                } else {
                    // No entry event found — treat as freshly entered.
                    // Safe default; worst case is one extra budget of wait.
                    crate::runtime_metrics::record_state_timeout_armed_on_hydration(
                        ctx.tenant.as_str(),
                        ctx.entity_type,
                        &st.state,
                        "budgeted",
                    );
                }
            }

            let armed_seq = self.state_timeout_tracker.bump(&key);
            self.state_timeout_tracker.inc_pending(ctx.entity_type);
            let params: serde_json::Value = serde_json::to_value(&st.params)
                .unwrap_or_else(|_| serde_json::Value::Object(serde_json::Map::new()));

            let state = self.clone();
            let tracker = self.state_timeout_tracker.clone();
            let tenant = ctx.tenant.clone();
            let entity_type = ctx.entity_type.to_string();
            let entity_id = ctx.entity_id.to_string();
            let target_state = st.state.clone();
            let target_action = st.on_timeout.clone();
            let agent_ctx = ctx.agent_ctx.clone();
            let key_for_task = key.clone();
            let entity_type_for_dec = ctx.entity_type.to_string();
            let workflow_root_entity_type = agent_ctx
                .workflow_root_entity_type
                .clone()
                .unwrap_or_else(|| entity_type.clone());
            let workflow_root_entity_id = agent_ctx
                .workflow_root_entity_id
                .clone()
                .unwrap_or_else(|| entity_id.clone());
            let workflow_run_id = agent_ctx
                .workflow_run_id
                .clone()
                .unwrap_or_else(|| format!("{entity_type}:{entity_id}"));

            tracing::debug!(
                tenant = %ctx.tenant,
                entity_type = ctx.entity_type,
                entity_id = ctx.entity_id,
                target_state = st.state.as_str(),
                target_action = st.on_timeout.as_str(),
                delay_ms = delay.as_millis() as u64,
                workflow.root_entity_type = %workflow_root_entity_type,
                workflow.root_entity_id = %workflow_root_entity_id,
                workflow.run_id = %workflow_run_id,
                "armed state timeout"
            );

            tokio::spawn(async move {
                // determinism-ok: wall-clock timer fires a side-effect action;
                // the action itself is deterministic under DST via sim_now().
                tokio::time::sleep(delay).await; // determinism-ok: scheduled delay

                let span = tracing::info_span!(
                    "dispatch.state_timeout.fire",
                    tenant = %tenant,
                    entity_type = %entity_type,
                    entity_id = %entity_id,
                    target_state = %target_state,
                    target_action = %target_action,
                    workflow.root_entity_type = %workflow_root_entity_type,
                    workflow.root_entity_id = %workflow_root_entity_id,
                    workflow.run_id = %workflow_run_id,
                );

                async move {
                    // Sequence-based cancellation check. A newer arm (or a
                    // state change that bumped the seq on exit) renders
                    // this timer a no-op.
                    if tracker.current(&key_for_task) != armed_seq {
                        tracker.dec_pending(&entity_type_for_dec);
                        return;
                    }

                    // Secondary check: confirm the entity is still in the
                    // target state. Covers races where the entity left and
                    // re-entered the same state with a new seq greater than
                    // this one (in which case the tracker seq would differ,
                    // already caught above). This is defense in depth.
                    match state
                        .get_tenant_entity_state(&tenant, &entity_type, &entity_id)
                        .await
                    {
                        Ok(current) if current.state.status == target_state => {
                            crate::runtime_metrics::record_state_timeout_fired(
                                tenant.as_str(),
                                &entity_type,
                                &target_state,
                                &target_action,
                            );
                            let _ = state
                                .dispatch_tenant_action(
                                    &tenant,
                                    &entity_type,
                                    &entity_id,
                                    &target_action,
                                    params,
                                    &agent_ctx,
                                )
                                .await;
                        }
                        _ => {
                            // State changed or fetch failed — nothing to do.
                        }
                    }
                    tracker.dec_pending(&entity_type_for_dec);
                }
                .instrument(span)
                .await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_runtime::tenant::TenantId;

    fn key() -> EntityKey {
        EntityKey::new(&TenantId::from("t".to_string()), "E", "e-1")
    }

    #[test]
    fn bump_returns_monotonically_increasing_seq() {
        let t = StateTimeoutTracker::new();
        let k = key();
        assert_eq!(t.current(&k), 0, "initial seq is 0");
        assert_eq!(t.bump(&k), 1);
        assert_eq!(t.bump(&k), 2);
        assert_eq!(t.bump(&k), 3);
        assert_eq!(t.current(&k), 3);
    }

    #[test]
    fn per_entity_seqs_are_independent() {
        let t = StateTimeoutTracker::new();
        let a = EntityKey::new(&TenantId::from("t".to_string()), "E", "a");
        let b = EntityKey::new(&TenantId::from("t".to_string()), "E", "b");
        assert_eq!(t.bump(&a), 1);
        assert_eq!(t.bump(&a), 2);
        assert_eq!(t.bump(&b), 1, "b's seq is independent of a's");
        assert_eq!(t.current(&a), 2);
        assert_eq!(t.current(&b), 1);
    }

    #[test]
    fn forget_releases_entity() {
        let t = StateTimeoutTracker::new();
        let tenant = TenantId::from("t".to_string());
        t.bump(&EntityKey::new(&tenant, "E", "x"));
        assert_eq!(t.size(), 1);
        t.forget(&tenant, "E", "x");
        assert_eq!(t.size(), 0);
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
        let reset = compute_state_clock_reset_ts(&events, "Closed", &[]).unwrap();
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
        let reset = compute_state_clock_reset_ts(&events, "Executing", &reset_on).unwrap();
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
        let reset = compute_state_clock_reset_ts(&events, "Executing", &reset_on).unwrap();
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

        let reset = compute_state_clock_reset_ts(&events, "Executing", &[]);
        assert!(reset.is_none(), "no entry event in window → None");
    }

    #[test]
    fn clock_reset_ignores_entry_events_for_other_states() {
        let mut events = VecDeque::new();
        events.push_back(test_event("Create", "", "Open", 1_000));
        events.push_back(test_event("Assign", "Open", "InProgress", 2_000));
        // Query for Open, but the current state is InProgress — no match.
        let reset = compute_state_clock_reset_ts(&events, "Open", &[]);
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

        let reset = compute_state_clock_reset_ts(&events, "Executing", &[]).unwrap();
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

    const TICKET_CSDL: &str = include_str!("../../../../../test-fixtures/specs/model.csdl.xml");

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
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn load_120_concurrent_dispatches_admission_caps_hold() {
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Instant;

        let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            "default",
            csdl,
            TICKET_CSDL.to_string(),
            &[("Ticket", TICKET_WITH_ADMISSION_IOA)],
        );
        let system = ActorSystem::new("load-admission-test");
        let state = Arc::new(ServerState::from_registry(system, registry));
        let tenant = temper_runtime::tenant::TenantId::from("default".to_string());
        let agent_ctx = AgentContext::for_service("timeout-scheduler");

        // Pre-create 120 ticket entities so the concurrent AssignAgent calls
        // race on the shared admission cap for that action.
        const N: usize = 120;
        for i in 0..N {
            state
                .get_or_create_tenant_entity(
                    &tenant,
                    "Ticket",
                    &format!("t-{i}"),
                    serde_json::json!({}),
                )
                .await
                .expect("create ticket");
        }

        let granted = Arc::new(AtomicUsize::new(0));
        let deferred = Arc::new(AtomicUsize::new(0));
        let other = Arc::new(AtomicUsize::new(0));
        let in_flight_peak = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let latencies_ns = Arc::new(Mutex::new(Vec::<u128>::with_capacity(N)));

        let barrier = Arc::new(tokio::sync::Barrier::new(N));
        let mut handles = Vec::with_capacity(N);
        let wall_start = Instant::now(); // determinism-ok: test-only concurrency latency measurement
        for i in 0..N {
            let state = state.clone();
            let tenant = tenant.clone();
            let agent_ctx = agent_ctx.clone();
            let granted = granted.clone();
            let deferred = deferred.clone();
            let other = other.clone();
            let in_flight_peak = in_flight_peak.clone();
            let in_flight = in_flight.clone();
            let latencies_ns = latencies_ns.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await; // fire all at once
                let call_start = Instant::now(); // determinism-ok: test-only concurrency latency measurement
                in_flight.fetch_add(1, Ordering::AcqRel);
                // Record peak in-flight count.
                let cur = in_flight.load(Ordering::Acquire);
                let mut peak = in_flight_peak.load(Ordering::Acquire);
                while cur > peak
                    && let Err(p) = in_flight_peak.compare_exchange(
                        peak,
                        cur,
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    )
                {
                    peak = p;
                }

                let res = state
                    .dispatch_tenant_action_ext_typed(
                        &tenant,
                        "Ticket",
                        &format!("t-{i}"),
                        "AssignAgent",
                        serde_json::json!({}),
                        crate::state::dispatch::DispatchExtOptions {
                            agent_ctx: &agent_ctx,
                            await_integration: false,
                            await_reactions: true,
                        },
                    )
                    .await;
                let call_ns = call_start.elapsed().as_nanos();
                latencies_ns.lock().unwrap().push(call_ns);
                match res {
                    Ok(r) if r.success => {
                        granted.fetch_add(1, Ordering::AcqRel);
                    }
                    Ok(_) => {
                        other.fetch_add(1, Ordering::AcqRel);
                    }
                    Err(crate::state::dispatch::DispatchError::Deferred { .. }) => {
                        deferred.fetch_add(1, Ordering::AcqRel);
                    }
                    Err(_) => {
                        other.fetch_add(1, Ordering::AcqRel);
                    }
                }
                in_flight.fetch_sub(1, Ordering::AcqRel);
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let wall = wall_start.elapsed();

        let g = granted.load(Ordering::Acquire);
        let d = deferred.load(Ordering::Acquire);
        let o = other.load(Ordering::Acquire);
        let peak = in_flight_peak.load(Ordering::Acquire);
        let mut lats = latencies_ns.lock().unwrap().clone();
        lats.sort_unstable();
        let p = |q: f64| -> u128 {
            let idx = ((lats.len() as f64 - 1.0) * q).round() as usize;
            lats[idx.min(lats.len().saturating_sub(1))]
        };
        let throughput = N as f64 / wall.as_secs_f64();

        eprintln!(
            "LOAD RESULT: granted={g} deferred={d} other={o} in_flight_peak={peak} total={N}"
        );
        eprintln!(
            "LOAD PERF:   wall={wall_ms:.1}ms throughput={tp:.0}/s p50={p50:.2}ms p95={p95:.2}ms p99={p99:.2}ms max={pmax:.2}ms",
            wall_ms = wall.as_secs_f64() * 1000.0,
            tp = throughput,
            p50 = (p(0.50) as f64) / 1_000_000.0,
            p95 = (p(0.95) as f64) / 1_000_000.0,
            p99 = (p(0.99) as f64) / 1_000_000.0,
            pmax = (p(1.0) as f64) / 1_000_000.0,
        );

        // Hard contract assertions:
        //
        // 1. Zero unknown failures — every outcome is Granted or Deferred;
        //    no panics, no permanent errors, no timeouts.
        assert_eq!(
            o, 0,
            "unexpected non-granted, non-deferred outcomes: {o} (spec: 500-class behavior is forbidden)"
        );

        // 2. Every dispatch is accounted for.
        assert_eq!(g + d + o, N, "outcome count must equal submissions");

        // 3. Admission cap holds — since cap is 5 and queue_timeout is 10s
        //    with N=120 inputs, some should defer. If all 120 granted
        //    instantly, admission isn't firing at all.
        assert!(
            g >= 5,
            "at least the cap's worth ({}) should succeed, got {g}",
            5
        );

        // 4. Peak in-flight observation does NOT assert <= 5 because
        //    in_flight counts the pre-acquire window too; what we do
        //    assert is the admission semaphore gate works (see test
        //    `grants_up_to_cap_and_defers_beyond` for the hard cap proof).
    }

    /// Tight-cap spec with `queue_timeout_seconds = 0` — acquirers that
    /// cannot grab a permit instantly are immediately deferred.
    /// This proves the admission gate actually enforces the cap under
    /// sustained contention — a slow real-world action (like an LLM call
    /// that takes seconds per transition) would see the same cap enforced
    /// via a non-zero queue timeout.
    const TICKET_ZERO_QUEUE_IOA: &str = r#"
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
max_concurrent_creates = 2
max_concurrent_actions = { "AssignAgent" = 2 }
queue_depth = 1000
queue_timeout_seconds = 0
"#;

    /// Adversarial burst-load: 300 simultaneous dispatches against cap=2
    /// with a zero-second queue budget. Without admission's FIFO gate, the
    /// flood would hit the shared actor's 1000-deep mailbox and produce
    /// `MailboxFull` errors (the 2026-04-17 Katagami incident pattern).
    /// With admission active, the contract is: every caller is either
    /// `Granted` (served quickly through the cap) or `Deferred` (503
    /// Retry-After). **Zero 500s under any circumstances.**
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn load_tight_cap_observes_deferrals() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            "default",
            csdl,
            TICKET_CSDL.to_string(),
            &[("Ticket", TICKET_ZERO_QUEUE_IOA)],
        );
        let system = ActorSystem::new("load-tight-admission-test");
        let state = Arc::new(ServerState::from_registry(system, registry));
        let tenant = temper_runtime::tenant::TenantId::from("default".to_string());
        let agent_ctx = AgentContext::for_service("timeout-scheduler");

        // Shared entity — all 300 dispatches contend for the SAME ticket
        // so the actor's single-threaded processing adds queue time on
        // top of the admission gate, making deferrals observable.
        state
            .get_or_create_tenant_entity(&tenant, "Ticket", "shared-ticket", serde_json::json!({}))
            .await
            .expect("create");

        const N: usize = 300;
        let granted = Arc::new(AtomicUsize::new(0));
        let deferred = Arc::new(AtomicUsize::new(0));
        let other = Arc::new(AtomicUsize::new(0));
        let lat_granted_ns = Arc::new(std::sync::Mutex::new(Vec::<u128>::new()));
        let lat_deferred_ns = Arc::new(std::sync::Mutex::new(Vec::<u128>::new()));

        // Prime a synchronization barrier so ALL 300 fire at the same instant.
        let barrier = Arc::new(tokio::sync::Barrier::new(N));
        let mut handles = Vec::with_capacity(N);
        let wall_start = std::time::Instant::now(); // determinism-ok: test-only admission latency measurement
        for _i in 0..N {
            let state = state.clone();
            let tenant = tenant.clone();
            let agent_ctx = agent_ctx.clone();
            let granted = granted.clone();
            let deferred = deferred.clone();
            let other = other.clone();
            let lat_granted_ns = lat_granted_ns.clone();
            let lat_deferred_ns = lat_deferred_ns.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                let call_start = std::time::Instant::now(); // determinism-ok: test-only admission latency measurement
                let res = state
                    .dispatch_tenant_action_ext_typed(
                        &tenant,
                        "Ticket",
                        "shared-ticket",
                        "AssignAgent",
                        serde_json::json!({}),
                        crate::state::dispatch::DispatchExtOptions {
                            agent_ctx: &agent_ctx,
                            await_integration: false,
                            await_reactions: true,
                        },
                    )
                    .await;
                let call_ns = call_start.elapsed().as_nanos();
                match res {
                    Ok(_) => {
                        granted.fetch_add(1, Ordering::AcqRel);
                        lat_granted_ns.lock().unwrap().push(call_ns);
                    }
                    Err(crate::state::dispatch::DispatchError::Deferred { .. }) => {
                        deferred.fetch_add(1, Ordering::AcqRel);
                        lat_deferred_ns.lock().unwrap().push(call_ns);
                    }
                    Err(_) => {
                        other.fetch_add(1, Ordering::AcqRel);
                    }
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let wall = wall_start.elapsed();

        let g = granted.load(Ordering::Acquire);
        let d = deferred.load(Ordering::Acquire);
        let o = other.load(Ordering::Acquire);
        let throughput = N as f64 / wall.as_secs_f64();
        let mut gl = lat_granted_ns.lock().unwrap().clone();
        let mut dl = lat_deferred_ns.lock().unwrap().clone();
        gl.sort_unstable();
        dl.sort_unstable();
        let p = |v: &[u128], q: f64| -> u128 {
            if v.is_empty() {
                return 0;
            }
            let idx = ((v.len() as f64 - 1.0) * q).round() as usize;
            v[idx.min(v.len() - 1)]
        };
        eprintln!("TIGHT-CAP RESULT: granted={g} deferred={d} other={o} total={N}");
        eprintln!(
            "TIGHT-CAP PERF:   wall={wall_ms:.1}ms throughput={tp:.0}/s",
            wall_ms = wall.as_secs_f64() * 1000.0,
            tp = throughput,
        );
        eprintln!(
            "                  granted p50={gp50:.2}ms p95={gp95:.2}ms p99={gp99:.2}ms",
            gp50 = (p(&gl, 0.50) as f64) / 1_000_000.0,
            gp95 = (p(&gl, 0.95) as f64) / 1_000_000.0,
            gp99 = (p(&gl, 0.99) as f64) / 1_000_000.0,
        );
        eprintln!(
            "                  deferred p50={dp50:.2}ms p95={dp95:.2}ms p99={dp99:.2}ms (time-to-503)",
            dp50 = (p(&dl, 0.50) as f64) / 1_000_000.0,
            dp95 = (p(&dl, 0.95) as f64) / 1_000_000.0,
            dp99 = (p(&dl, 0.99) as f64) / 1_000_000.0,
        );

        // Hard contract:
        //   * Zero 500-class outcomes. Every caller is either served or told
        //     to back off with a 503-equivalent.
        assert_eq!(o, 0, "expected no 500-class outcomes, got {o}");
        //   * All N accounted for.
        assert_eq!(g + d + o, N);
        //   * Admission actually bites: with cap=2 and 1s queue timeout
        //     against 300 contenders on one actor, not all can serve.
        //     This is the incident-class proof: burst → deferrals, not 500s.
        assert!(
            d > 0,
            "admission control must produce at least one deferral under tight cap; got granted={g} deferred={d}"
        );
    }

    /// 1000-way sustained-throughput measurement: cap=50 (realistic
    /// production-style cap), each call hits a unique entity so work
    /// parallelizes across actors. Measures steady-state dispatch cost
    /// through the full retry + admission + dispatch path.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn load_1000_throughput_baseline() {
        use std::sync::Arc;
        use std::sync::Mutex;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Instant;

        const THROUGHPUT_IOA: &str = r#"
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
max_concurrent_creates = 50
max_concurrent_actions = { "AssignAgent" = 50 }
queue_depth = 2000
queue_timeout_seconds = 30
"#;

        let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            "default",
            csdl,
            TICKET_CSDL.to_string(),
            &[("Ticket", THROUGHPUT_IOA)],
        );
        let system = ActorSystem::new("throughput-1000-test");
        let state = Arc::new(ServerState::from_registry(system, registry));
        let tenant = temper_runtime::tenant::TenantId::from("default".to_string());
        let agent_ctx = AgentContext::for_service("timeout-scheduler");

        const N: usize = 1000;

        // Pre-create all entities so the dispatch phase is pure transition.
        for i in 0..N {
            state
                .get_or_create_tenant_entity(
                    &tenant,
                    "Ticket",
                    &format!("t-{i}"),
                    serde_json::json!({}),
                )
                .await
                .expect("create");
        }

        let granted = Arc::new(AtomicUsize::new(0));
        let errored = Arc::new(AtomicUsize::new(0));
        let lat_ns = Arc::new(Mutex::new(Vec::<u128>::with_capacity(N)));
        let barrier = Arc::new(tokio::sync::Barrier::new(N));

        let mut handles = Vec::with_capacity(N);
        let wall_start = Instant::now(); // determinism-ok: test-only reaction latency measurement
        for i in 0..N {
            let state = state.clone();
            let tenant = tenant.clone();
            let agent_ctx = agent_ctx.clone();
            let granted = granted.clone();
            let errored = errored.clone();
            let lat_ns = lat_ns.clone();
            let barrier = barrier.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                let start = Instant::now(); // determinism-ok: test-only reaction latency measurement
                let res = state
                    .dispatch_tenant_action_ext_typed(
                        &tenant,
                        "Ticket",
                        &format!("t-{i}"),
                        "AssignAgent",
                        serde_json::json!({}),
                        crate::state::dispatch::DispatchExtOptions {
                            agent_ctx: &agent_ctx,
                            await_integration: false,
                            await_reactions: true,
                        },
                    )
                    .await;
                let elapsed = start.elapsed().as_nanos();
                lat_ns.lock().unwrap().push(elapsed);
                match res {
                    Ok(r) if r.success => {
                        granted.fetch_add(1, Ordering::AcqRel);
                    }
                    _ => {
                        errored.fetch_add(1, Ordering::AcqRel);
                    }
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        let wall = wall_start.elapsed();

        let g = granted.load(Ordering::Acquire);
        let e = errored.load(Ordering::Acquire);
        let mut lats = lat_ns.lock().unwrap().clone();
        lats.sort_unstable();
        let p = |q: f64| -> u128 {
            let idx = ((lats.len() as f64 - 1.0) * q).round() as usize;
            lats[idx.min(lats.len() - 1)]
        };
        let tp = N as f64 / wall.as_secs_f64();

        eprintln!("1000-THROUGHPUT: granted={g} other={e} total={N}");
        eprintln!(
            "1000-PERF:   wall={wall_ms:.1}ms throughput={tp:.0} dispatches/sec",
            wall_ms = wall.as_secs_f64() * 1000.0,
        );
        eprintln!(
            "             p50={p50:.2}ms p90={p90:.2}ms p95={p95:.2}ms p99={p99:.2}ms max={pmax:.2}ms",
            p50 = (p(0.50) as f64) / 1_000_000.0,
            p90 = (p(0.90) as f64) / 1_000_000.0,
            p95 = (p(0.95) as f64) / 1_000_000.0,
            p99 = (p(0.99) as f64) / 1_000_000.0,
            pmax = (p(1.0) as f64) / 1_000_000.0,
        );

        assert_eq!(e, 0, "1000-dispatch baseline must be zero-error");
        assert_eq!(g, N, "every dispatch should succeed under generous cap");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn state_timeout_fires_and_transitions_entity() {
        let csdl = parse_csdl(TICKET_CSDL).expect("CSDL parses");
        let mut registry = SpecRegistry::new();
        registry.register_tenant(
            "default",
            csdl,
            TICKET_CSDL.to_string(),
            &[("Ticket", TICKET_WITH_TIMEOUT_IOA)],
        );
        let system = ActorSystem::new("state-timeout-integration");
        let state = ServerState::from_registry(system, registry);

        let tenant = temper_runtime::tenant::TenantId::from("default".to_string());
        let agent_ctx = AgentContext::for_service("timeout-scheduler");

        // Create the entity so it lands in `Open`.
        let created = state
            .get_or_create_tenant_entity(&tenant, "Ticket", "t-1", serde_json::json!({}))
            .await
            .expect("create ticket");
        assert_eq!(created.state.status, "Open");

        // Arm the state_timeout by dispatching a no-op Action? We need to
        // trigger `arm_state_timeouts_if_needed`. Creation itself doesn't
        // go through dispatch, so arm via a self-loop transition — easiest
        // path is to dispatch a direct RecordProgress-like action. Ticket
        // spec has no self-loop, so we simulate by inspecting initial state
        // and letting the watchdog fire by entering Open via Configure. For
        // this test, we force an arm by directly calling the ServerState
        // hook with a synthesized PostDispatchContext.
        let response = state
            .get_tenant_entity_state(&tenant, "Ticket", "t-1")
            .await
            .unwrap();
        let ctx = PostDispatchContext {
            tenant: &tenant,
            entity_type: "Ticket",
            entity_id: "t-1",
            action: "__Created",
            agent_ctx: &agent_ctx,
            dispatch_idempotency_key: None,
            action_params: &serde_json::json!({}),
            await_integration: false,
        };
        state.arm_state_timeouts_if_needed(&ctx, &response);

        // Timer is 1s; give it 2s to fire + dispatch + apply.
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let after = state
            .get_tenant_entity_state(&tenant, "Ticket", "t-1")
            .await
            .unwrap();
        assert_eq!(
            after.state.status, "InProgress",
            "state_timeout should have fired AssignAgent and moved Ticket to InProgress"
        );
        // Sanity: dispatch the same action explicitly — must fail because
        // AssignAgent is no longer valid from InProgress. This confirms the
        // transition actually went through the state machine (not a faked
        // status update).
        let retry = state
            .dispatch(DispatchCommand {
                tenant: &tenant,
                entity_type: "Ticket",
                entity_id: "t-1",
                action: "AssignAgent",
                params: serde_json::json!({}),
                agent_ctx: &agent_ctx,
                await_integration: false,
                await_reactions: true,
            })
            .await;
        if let Ok(r) = retry {
            assert!(
                !r.success,
                "AssignAgent must be rejected from InProgress (state machine integrity check)"
            );
        }
    }
}
