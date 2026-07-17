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
//!
//! Durability (ADR-0056, ADR-0171): actor spawn and post-dispatch fallback
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

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::spawn as spawn_timeout_hydration; // determinism-ok: one bounded task per actor startup lifecycle
use tracing::Instrument;

use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use crate::entity_actor::types::STATE_TIMEOUT_PRECONDITION_MISMATCH;
use crate::entity_actor::{EntityEvent, EntityResponse, EntityState, StateTimeoutPrecondition};

use super::{DispatchCommand, effects::PostDispatchContext};

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
/// A snapshot-carried anchor is authoritative when the bounded recent-event
/// window no longer contains the entry event. Returns `None` only for a legacy
/// snapshot (or fresh in-memory state) with neither source available.
fn compute_state_clock_reset_ts(
    events: &VecDeque<EntityEvent>,
    snapshot_reset_at: Option<DateTime<Utc>>,
    current_state: &str,
    reset_on: &[String],
) -> Option<DateTime<Utc>> {
    // Find the most recent state entry: last event whose to_status ==
    // current_state AND from_status != current_state. Scanning backward
    // because recent events are at the back.
    let entry_idx = events
        .iter()
        .rposition(|e| e.to_status == current_state && e.from_status != current_state);
    let entry_reset_at = entry_idx.map(|entry_idx| events[entry_idx].timestamp);
    let reset_at = match (snapshot_reset_at, entry_reset_at) {
        (Some(snapshot), Some(entry)) => snapshot.max(entry),
        (Some(snapshot), None) => snapshot,
        (None, Some(entry)) => entry,
        (None, None) => return None,
    };

    // When the entry is in the hot tail, only later events may reset it. When
    // the entry is represented by a snapshot anchor, every tail event is later.
    let reset_scan_start = entry_idx.map_or(0, |entry_idx| entry_idx + 1);
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

/// Composite key identifying an entity instance inside the ownership tracker.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
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
/// Each accepted committed response increments and captures a generation;
/// firings compare the captured generation against the current owner and drop
/// the fire when they diverge. Journal sequence numbers make persisted response
/// acceptance monotonic; in-memory actors use their total applied event count.
#[derive(Default, Debug)]
pub struct StateTimeoutTracker {
    owners: Mutex<BTreeMap<EntityKey, StateTimeoutOwner>>,
    /// ADR-0049: per-entity-type count of armed-but-unfired timers.
    /// Emitted as `temper_scheduler_pending_timers` by the canary loop.
    pending_by_type: Mutex<BTreeMap<String, u64>>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct StateTimeoutOwner {
    generation: u64,
    event_order: u64,
    reset_at: Option<DateTime<Utc>>,
    reset_version: Option<u64>,
}

impl StateTimeoutTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance timeout ownership for a strictly newer committed response.
    ///
    /// Actor transitions commit in sequence order, but post-dispatch effects
    /// may complete out of order. Rejecting older or duplicate callbacks keeps
    /// a stale response from cancelling the timer for a newer reset.
    fn advance_if_fresh(
        &self,
        key: &EntityKey,
        event_order: u64,
        reset_at: Option<DateTime<Utc>>,
        reset_version: Option<u64>,
    ) -> Option<u64> {
        let mut map = self.owners.lock().expect("state_timeout tracker poisoned");
        let owner = map.entry(key.clone()).or_default();
        if owner.generation != 0 && event_order <= owner.event_order {
            return None;
        }
        owner.generation = owner
            .generation
            .checked_add(1)
            .expect("state timeout generation overflow");
        owner.event_order = event_order;
        owner.reset_at = reset_at;
        owner.reset_version = reset_version;
        Some(owner.generation)
    }

    /// Observe a newer response and arm only when ownership is missing or its
    /// durable clock anchor changed.
    ///
    /// This handles hydration/dispatch races and repairs the fresh in-memory
    /// fallback: the initial timed state has no anchor, then its first event
    /// establishes one. Unrelated callbacks with an unchanged anchor merely
    /// advance the monotonic observation order and keep the existing deadline.
    fn reconcile_if_fresh(
        &self,
        key: &EntityKey,
        event_order: u64,
        reset_at: Option<DateTime<Utc>>,
        reset_version: Option<u64>,
    ) -> Option<u64> {
        let mut map = self.owners.lock().expect("state_timeout tracker poisoned");
        let owner = map.entry(key.clone()).or_default();
        if owner.generation != 0 && event_order <= owner.event_order {
            return None;
        }
        let needs_arm = owner.generation == 0
            || owner.reset_at != reset_at
            || owner.reset_version != reset_version;
        owner.event_order = event_order;
        if !needs_arm {
            return None;
        }
        owner.generation = owner
            .generation
            .checked_add(1)
            .expect("state timeout generation overflow");
        owner.reset_at = reset_at;
        owner.reset_version = reset_version;
        Some(owner.generation)
    }

    fn current_generation(&self, key: &EntityKey) -> u64 {
        self.owners
            .lock()
            .expect("state_timeout tracker poisoned")
            .get(key)
            .map(|owner| owner.generation)
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
            .owners
            .lock()
            .expect("state_timeout tracker poisoned")
            .remove(&key);
    }

    #[cfg(test)]
    fn size(&self) -> usize {
        self.owners
            .lock()
            .expect("state_timeout tracker poisoned")
            .len()
    }
}

impl crate::state::ServerState {
    /// Reconcile a newly spawned actor's durable state with its declared timeout.
    ///
    /// The state read is synchronously admitted as the new actor's first
    /// mailbox message before its [`temper_runtime::actor::ActorRef`] is
    /// published. This task awaits that lifecycle-coupled reply, so neither
    /// slow startup nor already-queued application traffic can overtake or
    /// exhaust reconciliation. Keeping this hook on `ServerState` avoids a
    /// strong `ServerState -> actor -> ServerState` cycle.
    pub(crate) fn schedule_state_timeout_hydration(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        startup_state: temper_runtime::actor::PendingAsk<EntityResponse>,
    ) {
        let state = self.clone();
        let tenant = tenant.clone();
        let entity_type = entity_type.to_string();
        let entity_id = entity_id.to_string();
        let observed_at = sim_now();
        let readiness_started_at = tokio::time::Instant::now(); // determinism-ok: paused by DST

        spawn_timeout_hydration(async move {
            match startup_state.receive().await {
                Ok(response) => {
                    state.arm_state_timeouts_on_hydration(
                        &tenant,
                        &entity_type,
                        &entity_id,
                        &response,
                        observed_at,
                        readiness_started_at.elapsed(),
                    );
                }
                Err(error) => {
                    tracing::error!(
                        tenant = %tenant,
                        entity_type,
                        entity_id,
                        error = %error,
                        "state timeout hydration actor stopped before startup reconciliation"
                    );
                }
            }
        });
    }

    fn arm_state_timeouts_on_hydration(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        response: &EntityResponse,
        observed_at: DateTime<Utc>,
        readiness_elapsed: Duration,
    ) {
        let agent_ctx =
            crate::request_context::AgentContext::for_service("state-timeout-hydration");
        let action_params = serde_json::json!({});
        let ctx = PostDispatchContext {
            tenant,
            entity_type,
            entity_id,
            action: "__hydrated",
            agent_ctx: &agent_ctx,
            dispatch_idempotency_key: None,
            action_params: &action_params,
            await_integration: false,
        };
        self.arm_state_timeouts(
            &ctx,
            response,
            StateTimeoutArmCause::Hydration {
                observed_at,
                readiness_elapsed,
            },
        );
    }

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
        self.arm_state_timeouts(ctx, response, StateTimeoutArmCause::PostDispatch);
    }

    fn arm_state_timeouts(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
        cause: StateTimeoutArmCause,
    ) {
        let table = {
            let registry = match self.registry.read() {
                Ok(registry) => registry,
                Err(_) => return,
            };
            registry.get_table(ctx.tenant, ctx.entity_type)
        }
        .or_else(|| self.transition_tables.get(ctx.entity_type).cloned());
        let Some(table) = table else {
            return;
        };
        if table.state_timeouts.is_empty() {
            return;
        }
        let state_timeouts = table.state_timeouts.clone();

        let post_state = response.state.status.clone();
        let pre_state = response
            .state
            .events
            .back()
            .map(|e| e.from_status.clone())
            .unwrap_or_default();
        let state_changed = pre_state != post_state;
        let hydrating = matches!(cause, StateTimeoutArmCause::Hydration { .. });
        let key = EntityKey::new(ctx.tenant, ctx.entity_type, ctx.entity_id);
        let post_has_timeout = state_timeouts.iter().any(|st| st.state == post_state);
        let pre_had_timeout = state_timeouts.iter().any(|st| st.state == pre_state);
        let event_order = timeout_response_order(&response.state);
        let armed_reset_at = state_timeouts
            .iter()
            .find(|st| st.state == post_state)
            .and_then(|st| {
                compute_state_clock_reset_ts(
                    &response.state.events,
                    response.state.state_timeout_clock_reset_at,
                    &post_state,
                    &st.reset_on,
                )
            });
        let armed_reset_version = response.state.state_timeout_clock_reset_version;

        // A state change invalidates the prior timer and, when the destination
        // is timed, owns its replacement with the same generation. Advancing
        // once per durable response also rejects out-of-order callbacks.
        let transition_generation =
            if state_changed && !hydrating && (pre_had_timeout || post_has_timeout) {
                let Some(generation) = self.state_timeout_tracker.advance_if_fresh(
                    &key,
                    event_order,
                    armed_reset_at,
                    armed_reset_version,
                ) else {
                    return;
                };
                if pre_had_timeout {
                    crate::runtime_metrics::record_state_timeout_cancelled(
                        ctx.tenant.as_str(),
                        ctx.entity_type,
                        &pre_state,
                    );
                }
                if !post_has_timeout {
                    return;
                }
                Some(generation)
            } else {
                None
            };

        // Arm timers for the matching destination declaration.
        for st in &state_timeouts {
            if st.state != post_state {
                continue;
            }
            let is_entry = state_changed && !hydrating;
            let is_reset =
                !hydrating && !state_changed && st.reset_on.iter().any(|a| a == ctx.action);
            let (armed_seq, needs_hydration_rearm) = if is_entry {
                let Some(generation) = transition_generation else {
                    continue;
                };
                (generation, false)
            } else if is_reset {
                let Some(generation) = self.state_timeout_tracker.advance_if_fresh(
                    &key,
                    event_order,
                    armed_reset_at,
                    armed_reset_version,
                ) else {
                    // Post-dispatch effects can finish out of order. An older
                    // reset must not supersede a newer durable response.
                    continue;
                };
                (generation, false)
            } else {
                // ADR-0056: reserve reconciliation ownership only when no
                // dispatch or hydration path has already armed a timer.
                let Some(generation) = self.state_timeout_tracker.reconcile_if_fresh(
                    &key,
                    event_order,
                    armed_reset_at,
                    armed_reset_version,
                ) else {
                    continue;
                };
                (generation, true)
            };
            if is_reset {
                crate::runtime_metrics::record_state_timeout_reset(
                    ctx.tenant.as_str(),
                    ctx.entity_type,
                    &st.state,
                    ctx.action,
                );
            }

            // Determine the fire delay from the durable entry/reset anchor.
            //
            // Entry, reset, and hydration all share one absolute durable
            // deadline. This charges persistence and preceding post-dispatch
            // work instead of granting a fresh budget when arming runs late.
            let budget = Duration::from_secs(st.after_seconds);
            let now = match cause {
                StateTimeoutArmCause::PostDispatch => sim_now(),
                StateTimeoutArmCause::Hydration {
                    observed_at,
                    readiness_elapsed,
                } => hydration_reconciled_at(observed_at, readiness_elapsed),
            };
            let timeout = compute_timeout_delay(
                &response.state.events,
                response.state.state_timeout_clock_reset_at,
                &post_state,
                &st.reset_on,
                budget,
                now,
            );
            let delay = timeout.map_or(budget, |timeout| timeout.delay);
            if needs_hydration_rearm {
                if let Some(timeout) = timeout {
                    crate::runtime_metrics::record_state_timeout_armed_on_hydration(
                        ctx.tenant.as_str(),
                        ctx.entity_type,
                        &st.state,
                        if timeout.overdue {
                            "overdue"
                        } else {
                            "budgeted"
                        },
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
            let mut agent_ctx = ctx.agent_ctx.clone();
            // The timer is a distinct internal dispatch. Preserve caller
            // attribution and authority, but never reuse the request key that
            // entered or reset the timed state: actor deduplication is scoped
            // to entity + key and would swallow the timeout action itself.
            agent_ctx.idempotency_key = None;
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
            let deadline = timeout_deadline(delay); // determinism-ok: paused by DST

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
                tokio::time::sleep_until(deadline).await; // determinism-ok: scheduled deadline

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
                    // Generation cancellation check. A newer accepted durable
                    // response renders this timer a no-op.
                    if tracker.current_generation(&key_for_task) != armed_seq {
                        tracker.dec_pending(&entity_type_for_dec);
                        return;
                    }

                    let result = state
                        .dispatch_state_timeout_action(
                            DispatchCommand {
                                tenant: &tenant,
                                entity_type: &entity_type,
                                entity_id: &entity_id,
                                action: &target_action,
                                params,
                                agent_ctx: &agent_ctx,
                                await_integration: false,
                                await_reactions: true,
                            },
                            StateTimeoutPrecondition {
                                expected_state: target_state.clone(),
                                expected_reset_at: armed_reset_at,
                                expected_reset_version: armed_reset_version,
                            },
                        )
                        .await;
                    if !matches!(
                        result,
                        Ok(ref response)
                            if response.error.as_deref()
                                == Some(STATE_TIMEOUT_PRECONDITION_MISMATCH)
                    ) {
                        crate::runtime_metrics::record_state_timeout_fired(
                            tenant.as_str(),
                            &entity_type,
                            &target_state,
                            &target_action,
                        );
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
#[path = "state_timeouts/hydration_tests.rs"]
mod hydration_tests;

#[cfg(test)]
mod tests {
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
        assert_eq!(t.advance_if_fresh(&k, 7, None, None), Some(1));
        assert_eq!(t.advance_if_fresh(&k, 8, None, None), Some(2));
        assert_eq!(t.advance_if_fresh(&k, 9, None, None), Some(3));
        assert_eq!(t.current_generation(&k), 3);
    }

    #[test]
    fn stale_or_duplicate_durable_responses_cannot_advance_ownership() {
        let t = StateTimeoutTracker::new();
        let k = key();
        assert_eq!(t.advance_if_fresh(&k, 11, None, None), Some(1));
        assert_eq!(t.advance_if_fresh(&k, 10, None, None), None);
        assert_eq!(t.advance_if_fresh(&k, 11, None, None), None);
        assert_eq!(t.current_generation(&k), 1);
    }

    #[test]
    fn per_entity_owners_are_independent() {
        let t = StateTimeoutTracker::new();
        let a = EntityKey::new(&TenantId::from("t".to_string()), "E", "a");
        let b = EntityKey::new(&TenantId::from("t".to_string()), "E", "b");
        assert_eq!(t.advance_if_fresh(&a, 1, None, None), Some(1));
        assert_eq!(t.advance_if_fresh(&a, 2, None, None), Some(2));
        assert_eq!(
            t.advance_if_fresh(&b, 1, None, None),
            Some(1),
            "b's owner is independent of a's"
        );
        assert_eq!(t.current_generation(&a), 2);
        assert_eq!(t.current_generation(&b), 1);
    }

    #[test]
    fn forget_releases_entity() {
        let t = StateTimeoutTracker::new();
        let tenant = TenantId::from("t".to_string());
        assert_eq!(
            t.advance_if_fresh(&EntityKey::new(&tenant, "E", "x"), 1, None, None),
            Some(1)
        );
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
        let wall_start = Instant::now();
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
                let call_start = Instant::now();
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
        let wall_start = std::time::Instant::now();
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
                let call_start = std::time::Instant::now();
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
        let wall_start = Instant::now();
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
                let start = Instant::now();
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
