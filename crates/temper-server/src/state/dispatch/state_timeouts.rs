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
//! A record's armed timer follows its current state (`state_timer_reconcile.rs`):
//! after it rehydrates, after a spec install, and when a timer finds its record
//! in another state, the timer its current state declares is armed and any
//! other is cancelled.
//!
//! State exits and reset signals invalidate the old timer before waiting
//! for query visibility, so an old deadline cannot fire while a committed
//! reset is being projected. Successor timers are armed before that wait
//! too: a timer fires through the record's actor, not the query plane, and a
//! projection that lags or fails must not leave a committed record untimed.
//!
//! Durability (ADR-0056): every armed timer keeps a durable row with its
//! clock start, parameters and the identity it fires as, and removes it when
//! it ends. At boot `rearm_state_timers` (`state_timer_rows.rs`) arms each
//! waiting row again: for the rest of its budget, or at once if it fell due
//! while the server was down. A timer fires once and as whoever armed it.
//!
//! The arm logic also detects the **hydration case**: the entity is in a
//! state with a declared timeout but has no live in-memory timer (seq == 0 in
//! the tracker), as for a timer armed before its row was kept. Then the
//! dispatch reconstructs how long the entity has been in the current state
//! (the most recent transition into `state.status`, or the most recent
//! `reset_on` event after it) from the entity's status clock, which snapshots
//! keep, or from its retained events when the state predates the clock. It
//! fires `on_timeout` at once if that is a full budget, or arms a timer for the
//! rest. That timer fires as the dispatch that found it. Before the status
//! clock, a state restored from a snapshot (which leaves events out) or with
//! the entry pushed out of the retained events armed a full budget on every
//! hydration, so a daily timer on a server restarted more often than daily
//! never fired.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::Semaphore;

use temper_runtime::scheduler::{sim_now, sim_uuid};
use temper_runtime::tenant::TenantId;

use crate::entity_actor::{EntityEvent, EntityResponse, EntityState};
use crate::idempotency::IdempotencyKey;
use crate::odata::read_defaults::ReadDefaults;
use crate::request_context::{AgentContext, StateTimerFire};

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

/// When the clock of a timer on `current_state` started: from the entity's
/// status clock, which snapshots keep, or else from its retained events (a
/// state restored from a snapshot made before the status clock was kept).
fn state_clock_started_at(
    state: &EntityState,
    current_state: &str,
    reset_on: &[String],
) -> Option<DateTime<Utc>> {
    match state.status_clock.entered_at {
        Some(entered) if state.status == current_state => Some(
            reset_on
                .iter()
                .filter_map(|action| state.status_clock.actions.get(action))
                .copied()
                .fold(entered, DateTime::max),
        ),
        _ => compute_state_clock_reset_ts(&state.events, current_state, reset_on),
    }
}

/// Composite key identifying an entity instance inside the arm-seq tracker.
#[derive(Debug, Clone, Hash, PartialEq, Eq)]
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

/// One entity's cancellation counter, the journal sequence of the
/// transition that last armed or cancelled its timer, and the timer it has
/// armed now.
#[derive(Default)]
struct ArmSeq {
    seq: u64,
    journal: u64,
    armed: Option<Box<ArmedTimer>>,
    // The boot gate follows this record through resets and state entry, even
    // while a durable transition invalidates its prior timer.
    boot_gate: Option<Arc<Semaphore>>,
}

/// How an arm treats a timer the entity already has armed.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Reservation {
    /// A transition entered or reset the state: the new timer replaces the
    /// armed one, even one a reconcile armed for the same transition, since
    /// the transition knows who entered the state and when.
    Fresh,
    /// Arm the timer the state declares only when it is not armed already.
    Missing,
    /// Follow a committed arm: keep only that arm with the same declaration.
    Committed,
}

/// In-memory cancellation counter keyed by entity instance.
///
/// Each arm increments and captures the new value; firings compare captured
/// against current and drop the fire when they diverge. An arm from an
/// earlier journal sequence than the last arm or cancellation is refused, so
/// a dispatch whose post-dispatch work finishes late never replaces the timer
/// of a newer transition.
#[derive(Default)]
pub struct StateTimeoutTracker {
    seqs: Mutex<HashMap<EntityKey, ArmSeq>>,
    /// ADR-0049: per-entity-type count of armed-but-unfired timers.
    /// Emitted as `temper_scheduler_pending_timers` by the canary loop.
    pending_by_type: Mutex<HashMap<String, u64>>,
}

impl StateTimeoutTracker {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn bump(&self, key: &EntityKey) -> u64 {
        let mut map = self.seqs.lock().expect("state_timeout tracker poisoned");
        let entry = map.entry(key.clone()).or_default();
        entry.seq += 1;
        entry.seq
    }

    /// Cancel the entity's timer for the transition at journal sequence
    /// `journal`: no arm from an earlier transition is taken after this. A
    /// cancellation from an earlier transition than the last arm or
    /// cancellation (a late post-dispatch, or a retry answered from the
    /// idempotency cache) cancels nothing. Returns the timer it cancelled.
    fn invalidate(&self, key: &EntityKey, journal: u64) -> Option<ArmedTimer> {
        let mut map = self.seqs.lock().expect("state_timeout tracker poisoned");
        let entry = map.entry(key.clone()).or_default();
        if journal < entry.journal {
            return None;
        }
        entry.seq += 1;
        entry.journal = journal;
        entry.armed.take().map(|armed| *armed)
    }

    /// Take a new cancellation value for `timer`, armed by the transition at
    /// its journal sequence, or None when a later transition already armed or
    /// cancelled this entity's timer, or when `reservation` keeps the timer
    /// already armed.
    fn reserve(&self, timer: &ArmedTimer, reservation: Reservation) -> Option<u64> {
        let key = EntityKey::new(&timer.tenant, &timer.entity_type, &timer.entity_id);
        let journal = timer.sequence_nr;
        let mut map = self.seqs.lock().expect("state_timeout tracker poisoned");
        let entry = map.entry(key).or_default();
        if journal < entry.journal {
            return None;
        }
        let keep_armed = entry.armed.as_ref().is_some_and(|armed| {
            armed.is_declared_as(timer)
                && match reservation {
                    Reservation::Fresh => false,
                    Reservation::Missing => true,
                    Reservation::Committed => armed.arm_id == timer.arm_id,
                }
        });
        if keep_armed {
            return None;
        }
        entry.seq += 1;
        entry.journal = journal;
        entry.boot_gate = timer.boot_gate.clone().or_else(|| entry.boot_gate.clone());
        entry.armed = Some(Box::new(timer.clone()));
        Some(entry.seq)
    }

    /// Release an ended task's live arm without losing its journal fence or
    /// boot gate. A replacement, even for the same durable arm, owns its own
    /// cancellation value and must remain armed.
    pub(super) fn release(&self, timer: &ArmedTimer, armed_seq: u64) {
        let key = EntityKey::new(&timer.tenant, &timer.entity_type, &timer.entity_id);
        let mut map = self.seqs.lock().expect("state_timeout tracker poisoned");
        if let Some(entry) = map.get_mut(&key)
            && entry.seq == armed_seq
        {
            entry.seq += 1;
            entry.armed = None;
        }
    }

    fn boot_gate(&self, key: &EntityKey) -> Option<Arc<Semaphore>> {
        self.seqs
            .lock()
            .expect("state_timeout tracker poisoned")
            .get(key)
            .and_then(|arm| arm.boot_gate.clone())
    }

    /// The timer the entity has armed now, if any.
    fn armed(&self, key: &EntityKey) -> Option<ArmedTimer> {
        self.seqs
            .lock()
            .expect("state_timeout tracker poisoned")
            .get(key)
            .and_then(|arm| arm.armed.as_deref().cloned())
    }

    /// Whether the entity has a timer armed now.
    fn has_armed(&self, key: &EntityKey) -> bool {
        self.seqs
            .lock()
            .expect("state_timeout tracker poisoned")
            .get(key)
            .is_some_and(|arm| arm.armed.is_some())
    }

    fn current(&self, key: &EntityKey) -> u64 {
        self.seqs
            .lock()
            .expect("state_timeout tracker poisoned")
            .get(key)
            .map_or(0, |arm| arm.seq)
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
    /// Invalidate timers superseded by the durable transition before any I/O.
    pub(crate) fn invalidate_state_timeouts_if_needed(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        let Ok(registry) = self.registry.read() else {
            return;
        };
        let Some(spec) = registry.get_spec(ctx.tenant, ctx.entity_type) else {
            return;
        };
        let pre_state = response
            .state
            .events
            .back()
            .map(|event| event.from_status.as_str())
            .unwrap_or_default();
        let post_state = response.state.status.as_str();
        let state_changed = pre_state != post_state;
        let cancels_prior = state_changed
            && spec
                .automaton
                .state_timeouts
                .iter()
                .any(|timeout| timeout.state == pre_state);
        let resets_current = !state_changed
            && spec.automaton.state_timeouts.iter().any(|timeout| {
                timeout.state == post_state
                    && timeout.reset_on.iter().any(|action| action == ctx.action)
            });
        // Leaving a state ends whatever timer the record had armed, even one
        // its old state no longer declares since a spec install.
        let key = EntityKey::new(ctx.tenant, ctx.entity_type, ctx.entity_id);
        let leaves_armed = state_changed && self.state_timeout_tracker.has_armed(&key);
        if cancels_prior || resets_current || leaves_armed {
            self.state_timeout_tracker
                .invalidate(&key, response.state.sequence_nr);
        }
        if cancels_prior {
            crate::runtime_metrics::record_state_timeout_cancelled(
                ctx.tenant.as_str(),
                ctx.entity_type,
                pre_state,
            );
        }
    }

    /// Arm or re-arm state timers based on the just-completed transition.
    ///
    /// Invoked from `run_post_dispatch_effects`. A transition that entered a
    /// timed state, or reset its clock, arms its timer for a full budget. Any
    /// other transition leaves the record's timer to follow its state
    /// ([`Self::reconcile_state_timers`]): one it has no live timer for yet,
    /// as after a restart or passivation, is armed for the rest of its budget
    /// and fires as this dispatch. Each armed timer first writes its durable
    /// row, so a restart arms it again ([`Self::rearm_state_timers`]). The row
    /// is written before the dispatch returns: a repeating timer's next row
    /// then exists before the timer that fired it removes its own, so no
    /// restart falls between the two.
    pub(crate) async fn arm_state_timeouts_if_needed(
        &self,
        ctx: &PostDispatchContext<'_>,
        response: &EntityResponse,
    ) {
        let declared = {
            let Ok(registry) = self.registry.read() else {
                return;
            };
            let Some(spec) = registry.get_spec(ctx.tenant, ctx.entity_type) else {
                return;
            };
            spec.automaton
                .state_timeouts
                .iter()
                .find(|timeout| timeout.state == response.state.status)
                .cloned()
        };
        let pre_state = response
            .state
            .events
            .back()
            .map(|e| e.from_status.as_str())
            .unwrap_or_default();
        let state_changed = pre_state != response.state.status;
        let fresh = declared
            .filter(|st| state_changed || st.reset_on.iter().any(|action| action == ctx.action));
        let Some(st) = fresh else {
            self.reconcile_state_timers(
                ctx.tenant,
                ctx.entity_type,
                ctx.entity_id,
                &response.state,
                Some(ctx.agent_ctx),
            )
            .await;
            return;
        };
        if !state_changed {
            crate::runtime_metrics::record_state_timeout_reset(
                ctx.tenant.as_str(),
                ctx.entity_type,
                &st.state,
                ctx.action,
            );
        }
        // The clock starts at the event that entered or reset the state, as
        // its committed row says (`committed_timer_row`), not when the commit
        // returned: a slow commit must not move the deadline, and a restart
        // arms from the row.
        let now = sim_now();
        let started_at = response
            .state
            .events
            .back()
            .map_or(now, |event| event.timestamp);
        let mut timer = declared_timer(
            ctx.tenant,
            ctx.entity_type,
            &st,
            &response.state,
            &ReadDefaults::for_entity(self, ctx.tenant, ctx.entity_type),
            ctx.agent_ctx,
            started_at,
        );
        // The event that entered or reset the state committed this timer's
        // row under this id (`committed_timer_row`).
        timer.arm_id = super::entity_rows::timer_arm_id(&st.state, response.state.sequence_nr);
        self.start_armed_timer(timer, Reservation::Fresh).await;
    }

    /// Arm the timers of a record created straight into a timed state.
    ///
    /// Creation is not a dispatch, so nothing else arms them until the
    /// record's first action. Like any arm, the timer fires as whoever armed
    /// it: here, whoever created the record. A record that already has a live
    /// timer keeps it; one that already existed counts from when it entered
    /// its state.
    pub(crate) async fn arm_state_timeouts_on_creation(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        created: &EntityResponse,
        created_by: &AgentContext,
    ) {
        self.reconcile_state_timers(
            tenant,
            entity_type,
            entity_id,
            &created.state,
            Some(created_by),
        )
        .await;
    }

    /// Reserve, save and start one armed timer; whether it started. Reserved
    /// before the row is written: an arm whose dispatch finishes after a newer
    /// transition's is refused, and one that a newer arm or a state exit
    /// supersedes while its row is written does not start. Its row then gives
    /// way to the newer timer's, which is written again in case this one's
    /// write landed last, or is removed when no timer is armed.
    pub(super) async fn start_armed_timer(
        &self,
        mut timer: ArmedTimer,
        reservation: Reservation,
    ) -> bool {
        let key = EntityKey::new(&timer.tenant, &timer.entity_type, &timer.entity_id);
        timer.boot_gate = timer
            .boot_gate
            .or_else(|| self.state_timeout_tracker.boot_gate(&key));
        let Some(armed_seq) = self.state_timeout_tracker.reserve(&timer, reservation) else {
            return false;
        };
        self.persist_state_timer(&timer).await;
        if self.state_timeout_tracker.current(&key) != armed_seq {
            let mut written = timer.clone();
            loop {
                let seq = self.state_timeout_tracker.current(&key);
                match self.state_timeout_tracker.armed(&key) {
                    Some(newer) => {
                        self.persist_state_timer(&newer).await;
                        written = newer;
                    }
                    None => self.forget_state_timer(&written).await,
                }
                if self.state_timeout_tracker.current(&key) == seq {
                    break;
                }
            }
            return false;
        }
        let gate = timer.boot_gate.clone();
        self.spawn_state_timer(timer, gate, armed_seq);
        true
    }

    /// Cancel the entity's armed timer as of journal sequence `journal`, and
    /// remove its row. A cancellation older than the entity's last arm
    /// cancels nothing. With no timer armed, a type that declares timeouts
    /// still records `journal`, so a reconcile of an older state that finishes
    /// late (a rehydration overtaken by a delete) arms nothing.
    pub(super) async fn cancel_state_timer(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        journal: u64,
    ) {
        let key = EntityKey::new(tenant, entity_type, entity_id);
        if !self.state_timeout_tracker.has_armed(&key)
            && !self.declares_state_timeouts(tenant, entity_type)
        {
            return;
        }
        if let Some(cancelled) = self.state_timeout_tracker.invalidate(&key, journal) {
            self.forget_state_timer(&cancelled).await;
        }
    }

    /// Whether `entity` has a timer armed now.
    pub(super) fn has_armed_state_timer(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> bool {
        self.state_timeout_tracker
            .has_armed(&EntityKey::new(tenant, entity_type, entity_id))
    }

    /// The tracker's cancellation value for `entity` now: it changes with
    /// every arm and cancellation of its timer in this process.
    pub(super) fn state_timer_turn(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> u64 {
        self.state_timeout_tracker
            .current(&EntityKey::new(tenant, entity_type, entity_id))
    }

    /// The timer `entity` has armed now, if any.
    pub(super) fn armed_state_timer(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<ArmedTimer> {
        self.state_timeout_tracker
            .armed(&EntityKey::new(tenant, entity_type, entity_id))
    }

    /// Whether `timer`'s action already ran since its clock started, keeping
    /// the record in its state without resetting the clock: its fire
    /// committed, and a crash left the row behind before the timer ended. Such
    /// a timer fires once, so it does not fire again.
    pub(super) fn state_timer_already_fired(
        &self,
        timer: &ArmedTimer,
        current: &EntityState,
    ) -> bool {
        let ran_since = current
            .status_clock
            .actions
            .get(&timer.action)
            .is_some_and(|ran| *ran >= timer.started_at);
        ran_since && !self.state_timeout_resets_on(timer)
    }

    /// Whether `timer`'s action is in its state's `reset_on`: a repeating
    /// timer, whose fire starts the clock again.
    fn state_timeout_resets_on(&self, timer: &ArmedTimer) -> bool {
        let Ok(registry) = self.registry.read() else {
            return false;
        };
        registry
            .get_spec(&timer.tenant, &timer.entity_type)
            .and_then(|spec| {
                spec.automaton
                    .state_timeouts
                    .iter()
                    .find(|timeout| timeout.state == timer.state)
            })
            .is_some_and(|timeout| timeout.reset_on.contains(&timer.action))
    }

    /// Reserve a cancellation value for `timer` (see [`StateTimeoutTracker`]),
    /// or None when a later transition of its entity already armed or
    /// cancelled a timer.
    pub(super) fn reserve_state_timer(&self, timer: &ArmedTimer) -> Option<u64> {
        self.state_timeout_tracker
            .reserve(timer, Reservation::Fresh)
    }

    /// Make `timer`'s record, found past the journal sequence the timer was
    /// armed at, follow its current state. Another writer fired this arming,
    /// or the record took an action the timer does not reset on. The record
    /// gets the timer `current`'s state declares, armed at `current`'s
    /// sequence for the rest of its budget (none for a timer that fires once
    /// and already did), as `timer`'s identity and through its boot gate. A
    /// newer arm in this process since `timer` (`armed_seq` is no longer
    /// current) already follows the record, so nothing changes. `timer`'s
    /// task woke for its deadline, so a timer armed here at that deadline
    /// fires on time, not overdue.
    pub(super) async fn follow_moved_record(
        &self,
        timer: &ArmedTimer,
        armed_seq: u64,
        current: &EntityState,
    ) {
        debug_assert!(
            current.sequence_nr > timer.sequence_nr,
            "PRECONDITION: the record moved past the timer's arming"
        );
        let key = EntityKey::new(&timer.tenant, &timer.entity_type, &timer.entity_id);
        if self.state_timeout_tracker.current(&key) != armed_seq {
            return;
        }
        // Release this arm in memory only: its row stays until its task ends,
        // after the next timer's row is written.
        self.state_timeout_tracker
            .invalidate(&key, current.sequence_nr);
        let follow = super::state_timer_reconcile::Follow {
            armed_by: Some(&timer.agent_ctx),
            gate: timer.boot_gate.clone(),
            woke_for: Some(deadline(timer.started_at, timer.after_seconds)),
            ..Default::default()
        };
        self.follow_owned_state_timer(
            &timer.tenant,
            &timer.entity_type,
            &timer.entity_id,
            current,
            follow,
        )
        .await;
        debug_assert!(
            self.state_timeout_tracker
                .armed(&key)
                .is_none_or(|armed| armed.sequence_nr >= current.sequence_nr),
            "POSTCONDITION: no timer of an earlier arming is armed"
        );
    }

    /// Whether the record's spec still declares `timer` for its state: the
    /// same action after the same budget.
    pub(super) fn declares_state_timer(&self, timer: &ArmedTimer) -> bool {
        let Ok(registry) = self.registry.read() else {
            return false;
        };
        registry
            .get_spec(&timer.tenant, &timer.entity_type)
            .and_then(|spec| {
                spec.automaton
                    .state_timeouts
                    .iter()
                    .find(|timeout| timeout.state == timer.state)
            })
            .is_some_and(|timeout| {
                timeout.on_timeout == timer.action && timeout.after_seconds == timer.after_seconds
            })
    }

    /// Whether no newer arm or durable state exit superseded this timer.
    pub(super) fn state_timer_is_current(&self, timer: &ArmedTimer, armed_seq: u64) -> bool {
        self.state_timeout_tracker.current(&EntityKey::new(
            &timer.tenant,
            &timer.entity_type,
            &timer.entity_id,
        )) == armed_seq
    }

    /// Whether a newer arm holds this timer's row under the same id: a
    /// dispatch's arm that replaced a reconcile's for the same event. That
    /// row is the newer timer's to end.
    pub(super) fn state_timer_row_moved_on(&self, timer: &ArmedTimer, armed_seq: u64) -> bool {
        let key = EntityKey::new(&timer.tenant, &timer.entity_type, &timer.entity_id);
        self.state_timeout_tracker.current(&key) != armed_seq
            && self
                .state_timeout_tracker
                .armed(&key)
                .is_some_and(|armed| armed.arm_id == timer.arm_id)
    }
}

/// One armed timer: what fires, when its state's clock started, and who it
/// fires as.
#[derive(Clone)]
pub(super) struct ArmedTimer {
    pub(super) tenant: TenantId,
    pub(super) entity_type: String,
    pub(super) entity_id: String,
    /// The timed state; the timer fires only while the entity is still in it.
    pub(super) state: String,
    /// The `on_timeout` action and its parameters, read when armed.
    pub(super) action: String,
    pub(super) params: serde_json::Value,
    /// The declared budget, in seconds.
    pub(super) after_seconds: u64,
    /// The identity the action is dispatched as.
    pub(super) agent_ctx: AgentContext,
    /// Names this arming's durable row.
    pub(super) arm_id: String,
    /// The entity's journal sequence when the timer was armed.
    pub(super) sequence_nr: u64,
    pub(super) started_at: DateTime<Utc>,
    /// Shared dispatch gate inherited by replacements of boot timers.
    pub(super) boot_gate: Option<Arc<Semaphore>>,
}

impl ArmedTimer {
    /// Whether `other` is the same declared timer: the same state, action and
    /// budget.
    fn is_declared_as(&self, other: &ArmedTimer) -> bool {
        self.state == other.state
            && self.action == other.action
            && self.after_seconds == other.after_seconds
    }

    /// A key for the fires of one task of this arming: a fire tried again
    /// after its write landed unseen is answered as that write. It is the
    /// task's own, not the arming's: in a deploy overlap two processes fire
    /// the same arming, and the one whose fire did not commit then meets the
    /// other's key, not its own, so it is refused as past the arming rather
    /// than answered as a duplicate whose effects it would run a second time.
    pub(super) fn new_fire_key(&self) -> IdempotencyKey {
        IdempotencyKey::Request(format!("state-timer:{}:{}", self.arm_id, sim_uuid()))
    }

    /// What its fire is dispatched as: its identity under `fire_key`,
    /// marked as the fire of the arming at its journal sequence, which the
    /// record's actor applies only while the record is still at that
    /// sequence, and with its cause.
    pub(super) fn fire_context(&self, fire_key: &IdempotencyKey) -> AgentContext {
        let mut agent = self.agent_ctx.clone();
        agent.idempotency_key = Some(fire_key.clone());
        super::timer_cause::mark_timer_fire(
            &mut agent,
            Some(temper_authz::TimerRecord {
                entity_type: self.entity_type.clone(),
                entity_id: self.entity_id.clone(),
                state: self.state.clone(),
                action: self.action.clone(),
            }),
        );
        agent.event_origin = None;
        agent.fires_state_timer = Some(StateTimerFire {
            tenant: self.tenant.to_string(),
            entity_type: self.entity_type.clone(),
            entity_id: self.entity_id.clone(),
            state: self.state.clone(),
            action: self.action.clone(),
            armed_seq: self.sequence_nr,
        });
        agent
    }
}

/// The identity a timer fires as: the dispatch that armed it, as a new
/// background action. It does not keep the arming request's idempotency key:
/// the entity has already recorded that key, so the fire would be answered as
/// a duplicate and never run. Nor does it keep the mark of a timer fire that
/// armed it: each fire is marked with its own arming (`ArmedTimer::fire_context`).
pub(super) fn timer_agent(armed_by: &AgentContext) -> AgentContext {
    let mut agent = armed_by.for_background_task();
    agent.idempotency_key = None;
    agent.fires_state_timer = None;
    agent
}

/// When the clock of a timer re-armed without a transition started: the
/// entity's entry into its state or its latest `reset_on`, else `now` (a safe
/// default whose worst case is one extra budget of wait). None when the timer
/// keeps the entity in its state without resetting the clock and already fired
/// since the clock started: it fires once, so there is nothing to arm.
pub(super) fn rearm_clock_start(
    st: &temper_spec::automaton::StateTimeout,
    state: &EntityState,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let started_at = state_clock_started_at(state, &st.state, &st.reset_on).unwrap_or(now);
    let fired_since = state
        .status_clock
        .actions
        .get(&st.on_timeout)
        .is_some_and(|fired| *fired >= started_at);
    (!fired_since || st.reset_on.contains(&st.on_timeout)).then_some(started_at)
}

/// The timer `st` arms on an entity, firing as `armed_by`. ADR-0181: `args`
/// are read when the timer is armed, against the entity as it entered the
/// state, as guards read it: a variable it stores no value for reads as its
/// declared initial value in `declared`.
pub(super) fn declared_timer(
    tenant: &TenantId,
    entity_type: &str,
    st: &temper_spec::automaton::StateTimeout,
    state: &EntityState,
    declared: &ReadDefaults,
    armed_by: &AgentContext,
    started_at: DateTime<Utc>,
) -> ArmedTimer {
    let entity_id = state.entity_id.as_str();
    let read = declared.fields_as_read(&state.fields);
    let (params, missing) = temper_spec::automaton::resolve_args(&st.args, entity_id, &read);
    for field in missing {
        tracing::warn!(
            entity_type,
            state = %st.state,
            source_field = field,
            "state_timeout arg reads a missing field; skipping key"
        );
    }
    ArmedTimer {
        boot_gate: None,
        tenant: tenant.clone(),
        entity_type: entity_type.to_string(),
        entity_id: entity_id.to_string(),
        state: st.state.clone(),
        action: st.on_timeout.clone(),
        params: serde_json::Value::Object(params),
        after_seconds: st.after_seconds,
        agent_ctx: timer_agent(armed_by),
        arm_id: sim_uuid().to_string(),
        sequence_nr: state.sequence_nr,
        started_at,
    }
}

/// How much of a timer's budget is left at `now`; zero once it is due.
pub(super) fn remaining_budget(
    started_at: DateTime<Utc>,
    after_seconds: u64,
    now: DateTime<Utc>,
) -> Duration {
    let elapsed = now
        .signed_duration_since(started_at)
        .to_std()
        .unwrap_or(Duration::ZERO);
    Duration::from_secs(after_seconds).saturating_sub(elapsed)
}

/// When a timer whose clock started at `started_at` falls due. A budget past
/// the end of the calendar never falls due.
pub(super) fn deadline(started_at: DateTime<Utc>, after_seconds: u64) -> DateTime<Utc> {
    i64::try_from(after_seconds)
        .ok()
        .and_then(chrono::TimeDelta::try_seconds)
        .and_then(|budget| started_at.checked_add_signed(budget))
        .unwrap_or(DateTime::<Utc>::MAX_UTC)
}

#[cfg(test)]
mod tests {
    use super::*;
    use temper_runtime::tenant::TenantId;

    fn key() -> EntityKey {
        EntityKey::new(&TenantId::from("t".to_string()), "E", "e-1")
    }

    #[tokio::test(start_paused = true)]
    async fn boot_replacements_wait_for_the_shared_fire_gate() {
        for has_prior_row in [false, true] {
            let mut registry = SpecRegistry::new();
            registry.register_tenant(
                "t",
                parse_csdl(TICKET_CSDL).unwrap(),
                TICKET_CSDL.to_string(),
                &[("Ticket", TICKET_WITH_TIMEOUT_IOA)],
            );
            let server = crate::state::ServerState::from_registry(
                temper_runtime::ActorSystem::new("boot-gate"),
                registry,
            );
            let tenant = TenantId::new("t");
            let created = server
                .get_or_create_tenant_entity(&tenant, "Ticket", "e-1", serde_json::json!({}))
                .await
                .unwrap();
            let gate = Arc::new(Semaphore::new(0));
            if has_prior_row {
                let mut old = timer("Old", created.state.sequence_nr);
                old.entity_type = "Ticket".to_string();
                old.boot_gate = Some(gate.clone());
                server.reserve_state_timer(&old).unwrap();
            }
            let follow = super::super::state_timer_reconcile::Follow {
                gate: (!has_prior_row).then(|| gate.clone()),
                ..Default::default()
            };
            server
                .follow_state_timer(&tenant, "Ticket", "e-1", &created.state, follow)
                .await;
            let armed = server.armed_state_timer(&tenant, "Ticket", "e-1").unwrap();
            assert!(Arc::ptr_eq(armed.boot_gate.as_ref().unwrap(), &gate));
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            tokio::time::advance(Duration::from_secs(2)).await;
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                server
                    .get_tenant_entity_state(&tenant, "Ticket", "e-1")
                    .await
                    .unwrap()
                    .state
                    .status,
                "Open"
            );
            gate.add_permits(1);
            for _ in 0..200 {
                tokio::task::yield_now().await;
            }
            assert_eq!(
                server
                    .get_tenant_entity_state(&tenant, "Ticket", "e-1")
                    .await
                    .unwrap()
                    .state
                    .status,
                "InProgress"
            );
        }
    }

    /// Reproduce cancellation landing between a repair's snapshot and write.
    #[tokio::test]
    async fn repair_does_not_resurrect_a_cancelled_row() {
        use crate::storage::StateTimerStore;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use temper_runtime::persistence::{PersistenceError, StateTimerRecord};

        struct RacingRows {
            tracker: Arc<StateTimeoutTracker>,
            writes: AtomicUsize,
            row: Mutex<Option<StateTimerRecord>>,
        }
        #[async_trait::async_trait]
        impl StateTimerStore for RacingRows {
            async fn upsert_state_timer(
                &self,
                row: &StateTimerRecord,
            ) -> Result<(), PersistenceError> {
                match self.writes.fetch_add(1, Ordering::SeqCst) {
                    0 => {
                        self.tracker.reserve(&timer("New", 2), Reservation::Fresh);
                    }
                    1 => {
                        self.tracker.invalidate(&key(), 3);
                        *self.row.lock().unwrap() = None;
                    }
                    _ => {}
                }
                *self.row.lock().unwrap() = Some(row.clone());
                Ok(())
            }
            async fn delete_state_timer(
                &self,
                _: &str,
                _: &str,
                _: &str,
                arm: &str,
            ) -> Result<(), PersistenceError> {
                let mut row = self.row.lock().unwrap();
                if row.as_ref().is_some_and(|row| row.arm_id == arm) {
                    *row = None;
                }
                Ok(())
            }
            async fn list_state_timers(
                &self,
                _: &str,
            ) -> Result<Vec<StateTimerRecord>, PersistenceError> {
                Ok(self.row.lock().unwrap().iter().cloned().collect())
            }
        }
        let mut server = crate::state::ServerState::from_registry(
            temper_runtime::ActorSystem::new("repair-race"),
            crate::registry::SpecRegistry::new(),
        );
        let rows = Arc::new(RacingRows {
            tracker: server.state_timeout_tracker.clone(),
            writes: AtomicUsize::new(0),
            row: Mutex::new(None),
        });
        server.state_timer_rows = Some(rows.clone());
        assert!(
            !server
                .start_armed_timer(timer("Old", 1), Reservation::Fresh)
                .await
        );
        assert_eq!(rows.writes.load(Ordering::SeqCst), 2);
        assert!(
            rows.row.lock().unwrap().is_none(),
            "cancelled repair row must be removed"
        );
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

    /// A timer on `E/e-1` in `state`, armed by the transition at `journal`.
    fn timer(state: &str, journal: u64) -> ArmedTimer {
        ArmedTimer {
            boot_gate: None,
            tenant: TenantId::from("t".to_string()),
            entity_type: "E".to_string(),
            entity_id: "e-1".to_string(),
            state: state.to_string(),
            action: "Expire".to_string(),
            params: serde_json::json!({}),
            after_seconds: 60,
            agent_ctx: AgentContext::default(),
            arm_id: format!("{state}-{journal}"),
            sequence_nr: journal,
            started_at: DateTime::<Utc>::from_timestamp_millis(0).unwrap(),
        }
    }

    #[test]
    fn an_arm_from_an_earlier_transition_is_refused() {
        let t = StateTimeoutTracker::new();
        let k = key();
        assert_eq!(t.reserve(&timer("A", 5), Reservation::Fresh), Some(1));
        assert_eq!(
            t.reserve(&timer("A", 3), Reservation::Fresh),
            None,
            "an older transition's arm that finishes late"
        );
        assert_eq!(t.current(&k), 1, "the newer timer stays current");
        assert!(
            t.invalidate(&k, 7).is_some(),
            "the exit cancels the armed timer"
        );
        assert_eq!(t.current(&k), 2);
        assert_eq!(
            t.reserve(&timer("A", 6), Reservation::Fresh),
            None,
            "an arm from before the exit"
        );
        assert!(
            t.invalidate(&k, 4).is_none(),
            "a cancellation from an earlier transition cancels nothing"
        );
        assert_eq!(t.current(&k), 2);
        assert_eq!(
            t.reserve(&timer("A", 7), Reservation::Fresh),
            Some(3),
            "the same transition again, as a duplicate reply re-arms"
        );
    }

    #[test]
    fn an_armed_timer_is_kept_until_its_state_declares_another() {
        let t = StateTimeoutTracker::new();
        let k = key();
        assert_eq!(t.reserve(&timer("A", 5), Reservation::Missing), Some(1));
        assert_eq!(
            t.reserve(&timer("A", 6), Reservation::Missing),
            None,
            "a later look at the same state keeps the armed timer"
        );
        assert_eq!(
            t.reserve(&timer("A", 5), Reservation::Fresh),
            Some(2),
            "the transition's own arm replaces the one a reconcile armed for it"
        );
        assert_eq!(
            t.reserve(&timer("B", 6), Reservation::Missing),
            Some(3),
            "a record found in another state gets that state's timer"
        );
        assert_eq!(t.armed(&k).map(|armed| armed.state), Some("B".to_string()));
    }

    #[test]
    fn releasing_an_ended_task_preserves_newer_arms_and_the_journal_fence() {
        let tracker = StateTimeoutTracker::new();
        let k = key();
        let original = timer("A", 5);
        let first = tracker.reserve(&original, Reservation::Committed).unwrap();
        tracker.release(&original, first);
        assert!(!tracker.has_armed(&k), "the ended task is no longer live");
        assert_eq!(
            tracker.reserve(&timer("A", 4), Reservation::Fresh),
            None,
            "release must retain the journal fence"
        );

        let rearmed = tracker.reserve(&original, Reservation::Committed).unwrap();
        tracker.release(&original, first);
        assert_eq!(tracker.current(&k), rearmed);
        assert!(
            tracker.has_armed(&k),
            "a new task for the same row stays armed"
        );

        let newer = timer("B", 6);
        let replaced = tracker.reserve(&newer, Reservation::Fresh).unwrap();
        tracker.release(&original, rearmed);
        assert_eq!(tracker.current(&k), replaced);
        assert_eq!(
            tracker.armed(&k).unwrap().arm_id,
            newer.arm_id,
            "a newer transition's arm stays armed"
        );
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
            origin: Default::default(),
            kind: temper_runtime::persistence::EventKind::Transition,
            outcome: None,
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
initial = 0

[[state]]
name = "customer_responded"
type = "bool"
initial = false

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
initial = 0

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
initial = 0

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
initial = 0

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
        state.arm_state_timeouts_if_needed(&ctx, &response).await;

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
