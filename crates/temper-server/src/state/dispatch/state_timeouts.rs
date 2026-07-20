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
//! Cancellation is generation-based and ordered by the actor's committed event
//! order (durable sequence for journaled actors, total event count otherwise).
//! A post-dispatch callback can advance [`StateTimeoutTracker`] only when its
//! response is newer than the last accepted response. Every accepted arm
//! captures the new generation. When the timer fires, its action carries the
//! armed state and durable reset anchor into the actor; the actor validates both
//! atomically before applying the transition.
//!
//! Cancellation on state exit is implicit: before arming a new timer, we
//! advance ownership once for the transition. That advance renders any
//! in-flight timer for the old state stale and, when the destination is timed,
//! owns the replacement timer without a second generation change.
//! Ownership includes the exact timeout declaration. Registry-backed tasks
//! subscribe to ordered table-version changes so declaration replacement or
//! removal reconciles immediately, including while delivery is retrying.
//! Every live and hydrated timer executes under the same named internal
//! service principal and inherits only caller observability context.
//!
//! Durability (ADR-0056, ADR-0191): actor spawn and post-dispatch fallback
//! reconcile the **hydration case** — the entity is in a state with a
//! declared timeout but has no live in-memory timer. Reconciliation claims
//! the initial sequence atomically, reconstructs how long the entity has
//! been in the current state from the event log (the most recent transition
//! into `state.status`, or the most recent `reset_on` event after it), and:
//!   - if elapsed ≥ `after_seconds` → fire `on_timeout` immediately
//!     (the entity was overdue before this process ever saw it).
//!   - otherwise → arm a tokio timer with the remaining budget
//!     (`after_seconds - elapsed`).
//!
//! This closes the gap where an orphaned entity (actor passivated or server
//! restarted while in a timed state) would otherwise never have its timer
//! re-armed because no state transition happened on the hydrated actor.

use std::collections::VecDeque;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tracing::Instrument;

use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;
use temper_spec::automaton::StateTimeout;

use crate::entity_actor::types::STATE_TIMEOUT_PRECONDITION_MISMATCH;
use crate::entity_actor::{
    EntityEvent, EntityMsg, EntityResponse, EntityState, StateTimeoutPrecondition,
};

use super::{DispatchCommand, DispatchError, effects::PostDispatchContext};

mod arming;
mod declaration;
mod reconciliation;
mod tracker;

#[cfg(test)]
use reconciliation::StateTimeoutHydrationTiming;
pub(crate) use tracker::InactiveStateTimeoutFence;
pub use tracker::StateTimeoutTracker;
use tracker::{EntityKey, StateTimeoutPermit};

const STATE_TIMEOUT_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(100);
const STATE_TIMEOUT_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const STATE_TIMEOUT_SERVICE: &str = "state-timeout-hydration";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StateTimeoutArmCause {
    PostDispatch,
    Hydration {
        observed_at: DateTime<Utc>,
        readiness_elapsed: Duration,
    },
}

/// Walk the event history backward to find the timestamp of the most recent
/// "progress" signal for the given state: either the transition that entered
/// `current_state`, or any later event whose action is in `reset_on`. This
/// timestamp is the reference point for computing how long the entity has
/// been idle in the current state, used by the hydration re-arm path to
/// compute remaining timeout budget.
///
/// The actor-carried anchor is authoritative whenever present: current journal
/// payloads co-commit the table-at-commit clock outcome with every event, and
/// replay advances this field from that durable outcome. Re-scanning those
/// events with a later `reset_on` definition would reinterpret history. The
/// bounded event scan is therefore only a legacy/fresh-state fallback.
fn compute_state_clock_reset_ts(
    events: &VecDeque<EntityEvent>,
    durable_reset_at: Option<DateTime<Utc>>,
    current_state: &str,
    reset_on: &[String],
) -> Option<DateTime<Utc>> {
    if let Some(reset_at) = durable_reset_at {
        return Some(reset_at);
    }

    // Find the most recent state entry: last event whose to_status ==
    // current_state AND from_status != current_state. Scanning backward
    // because recent events are at the back.
    let entry_idx = events
        .iter()
        .rposition(|e| e.to_status == current_state && e.from_status != current_state)?;
    let reset_at = events[entry_idx].timestamp;

    // Only actions committed after this retained entry may reset it.
    let reset_scan_start = entry_idx + 1;
    let latest_reset_ts = events
        .iter()
        .skip(reset_scan_start)
        .filter(|e| reset_on.iter().any(|a| a == &e.action))
        .map(|e| e.timestamp)
        .max();

    Some(latest_reset_ts.map_or(reset_at, |latest| reset_at.max(latest)))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TimeoutDelay {
    delay: Duration,
    overdue: bool,
}

fn hydration_reconciled_at(
    observed_at: DateTime<Utc>,
    readiness_elapsed: Duration,
) -> DateTime<Utc> {
    let elapsed = chrono::Duration::from_std(readiness_elapsed).unwrap_or(chrono::Duration::MAX);
    observed_at
        .checked_add_signed(elapsed)
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

fn timeout_deadline(delay: Duration) -> tokio::time::Instant {
    tokio::time::Instant::now() + delay // determinism-ok: paused by DST
}

async fn wait_until_or_cancelled(
    cancellation: &mut tokio::sync::watch::Receiver<bool>,
    deadline: tokio::time::Instant,
) -> bool {
    if *cancellation.borrow() {
        return false;
    }
    tokio::select! { // determinism-ok: owner cancellation is ordered ahead of its deadline
        biased;
        _ = cancellation.changed() => false,
        _ = tokio::time::sleep_until(deadline) => !*cancellation.borrow(), // determinism-ok: scheduled deadline
    }
}

fn state_timeout_retry_delay(failure_count: u32, retry_after_ms: Option<u64>) -> Duration {
    debug_assert!(failure_count > 0, "retry delay requires a prior failure");
    let shift = failure_count.saturating_sub(1).min(9);
    let multiplier = 1_u32 << shift;
    let exponential = STATE_TIMEOUT_RETRY_INITIAL_DELAY
        .checked_mul(multiplier)
        .unwrap_or(STATE_TIMEOUT_RETRY_MAX_DELAY)
        .min(STATE_TIMEOUT_RETRY_MAX_DELAY);
    let requested = Duration::from_millis(retry_after_ms.unwrap_or(0));
    exponential
        .max(requested)
        .min(STATE_TIMEOUT_RETRY_MAX_DELAY)
}

fn compute_timeout_delay(
    events: &VecDeque<EntityEvent>,
    snapshot_reset_at: Option<DateTime<Utc>>,
    current_state: &str,
    reset_on: &[String],
    budget: Duration,
    now: DateTime<Utc>,
) -> Option<TimeoutDelay> {
    let reset_ts =
        compute_state_clock_reset_ts(events, snapshot_reset_at, current_state, reset_on)?;
    let elapsed = now
        .signed_duration_since(reset_ts)
        .to_std()
        .unwrap_or(Duration::ZERO);
    let overdue = elapsed >= budget;
    Some(TimeoutDelay {
        delay: budget.saturating_sub(elapsed),
        overdue,
    })
}

fn timeout_response_order(state: &EntityState) -> u64 {
    if state.sequence_nr != 0 {
        state.sequence_nr
    } else {
        u64::try_from(state.total_event_count).unwrap_or(u64::MAX)
    }
}

struct StateTimeoutWatch<'a> {
    key: &'a EntityKey,
    armed_generation: u64,
    tenant: &'a TenantId,
    entity_type: &'a str,
    entity_id: &'a str,
    target_state: &'a str,
    expected_timeout: &'a StateTimeout,
    agent_ctx: &'a crate::request_context::AgentContext,
}

impl crate::state::ServerState {
    /// Arm or re-arm state timers based on the just-completed transition.
    ///
    /// Invoked from `run_post_dispatch_effects`. Walks the spec's
    /// `state_timeouts`, advances monotonic per-entity ownership, and spawns a
    /// tokio task per armed timer. Actor-spawn hydration separately calls the
    /// same scheduler to recover durable deadlines after restart.
    pub(crate) fn arm_state_timeouts_if_needed(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        let Some(actor_uid) = ctx.actor_uid else {
            self.arm_state_timeouts(ctx, response, StateTimeoutArmCause::PostDispatch);
            return;
        };
        let actor_key = format!("{}:{}:{}", ctx.tenant, ctx.entity_type, ctx.entity_id);
        let registry = match self.actor_registry.read() {
            Ok(registry) => registry,
            Err(_) => {
                tracing::error!(
                    tenant = %ctx.tenant,
                    entity_type = ctx.entity_type,
                    entity_id = ctx.entity_id,
                    actor_uid = %actor_uid,
                    "actor registry lock poisoned while validating state-timeout ownership"
                );
                return;
            }
        };
        let is_current = registry
            .get(&actor_key)
            .is_some_and(|actor_ref| !actor_ref.is_stopped() && actor_ref.id().uid == actor_uid);
        if !is_current {
            tracing::debug!(
                tenant = %ctx.tenant,
                entity_type = ctx.entity_type,
                entity_id = ctx.entity_id,
                actor_uid = %actor_uid,
                "discarding state timeout callback from an evicted actor incarnation"
            );
            return;
        }
        self.arm_state_timeouts(ctx, response, StateTimeoutArmCause::PostDispatch);
        drop(registry);
    }

    fn current_state_timeout_declaration(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        target_state: &str,
    ) -> Option<StateTimeout> {
        let registry_table = self
            .registry
            .read()
            .expect("registry lock poisoned")
            .get_table(tenant, entity_type);
        registry_table
            .or_else(|| self.transition_tables.get(entity_type).cloned())
            .and_then(|table| {
                table
                    .state_timeouts
                    .iter()
                    .find(|timeout| timeout.state == target_state)
                    .cloned()
            })
    }
}

#[cfg(test)]
#[path = "state_timeouts/hydration_tests.rs"]
mod hydration_tests;

#[cfg(test)]
#[path = "state_timeouts/actor_eviction_race_tests.rs"]
mod actor_eviction_race_tests;
#[cfg(all(test, feature = "sim"))]
#[path = "state_timeouts/authoritative_delete_tests.rs"]
mod authoritative_delete_tests;
#[cfg(test)]
#[path = "state_timeouts/synthetic_hydration_race_tests.rs"]
mod synthetic_hydration_race_tests;

#[cfg(test)]
#[path = "state_timeouts/delivery_retry_tests.rs"]
mod delivery_retry_tests;

#[cfg(test)]
#[path = "state_timeouts/declaration_hotswap_tests.rs"]
mod declaration_hotswap_tests;

#[cfg(test)]
#[path = "state_timeouts/core_tests.rs"]
mod tests;
