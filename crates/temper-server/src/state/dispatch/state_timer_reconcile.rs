//! A record's armed timer follows its current state.
//!
//! [`ServerState::reconcile_state_timers`] is the one place that decides which
//! timer a record has armed: the one its current state declares, or none. It is
//! called whenever the record's state may have moved without a transition
//! arming or cancelling its timer:
//!
//! - after the record rehydrates (its actor starts from stored events), since
//!   a newer spec can declare another timer, or none, for the state it
//!   replays into than the one its timer was armed for;
//! - after a spec install, for every record of each type whose spec changed;
//! - when a timer fires on a record that is no longer in its state, or whose
//!   journal moved past the sequence the timer was armed at (another writer
//!   fired it, or an action it does not reset on came first);
//! - after a dispatch that neither entered nor reset a timed state, after a
//!   create, and after a delete.
//!
//! A fresh event arms a record that has no timer, and so no row: a dispatch,
//! a create, or an install that changed the record's spec. So does the boot
//! walk: a process can stop between an event and its row. A rehydration
//! follows only a timer the record already has, from its row
//! ([`Arming::FromRow`]).
//!
//! It is idempotent: a record whose armed timer is already the one its state
//! declares keeps it, so any number of calls leave one timer armed. The timer's
//! clock starts when the record entered its state, so a timer re-armed after a
//! restart or a rehydration waits only the rest of its budget, or fires at once.
//! A timer row that cannot be read leaves the record's timer as it is
//! ([`TimerFollow::RetryRead`]): it may hold the committed arm. A task that owns
//! the timer reads it again (`follow_owned_state_timer`).
//!
//! A timer fires as someone: the caller of the dispatch that reconciles, else
//! whoever armed the timer it replaces (its row's `armed_by`), else, when
//! nothing names a setter or only the state-timer service does (a record that
//! entered its state before rows were kept, or lost its row), the principal of
//! the record's newest move that names someone, as its journal keeps it
//! (`state_timer_setter`), else the kernel's [`STATE_TIMER_SERVICE`]. A
//! timeout action that binds its caller's identity (`authenticated_subject`)
//! refuses a service caller by design, so such a record waits for its next
//! move that names someone, whose caller arms it, and says so
//! (`state_timer_stop`).
//!
//! The walks that reconcile every record of a type, after an install and at
//! boot, are in `state_timer_follow`.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;

use super::state_timeouts::{
    Reservation, deadline, declared_timer, rearm_clock_start, remaining_budget,
};
use super::state_timer_follow::{FOLLOW_RETRY_FIRST, FOLLOW_RETRY_MAX};
use super::state_timer_setter::{STATE_TIMER_SERVICE, is_state_timer_service};
use crate::entity_actor::{EntityState, RehydrateHook};
use crate::odata::read_defaults::ReadDefaults;
use crate::request_context::AgentContext;
use crate::state::ServerState;

/// What a reconcile left a record with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimerFollow {
    /// The timer its state declares, or none.
    Settled,
    /// The record or its timer row could not be read, and its timer was left
    /// as it was: whoever owns the record's timer reads again.
    RetryRead,
    /// Its state declares a timer whose action binds its caller's identity,
    /// and nobody armed it: the record's next action arms it as that caller.
    WaitsForCaller,
    /// Its state declares a timer, but it has no timer and no row, and a
    /// rehydration does not arm one: its next action or the next boot does.
    WaitsForRow,
}

/// Whether a reconcile may arm a timer for a record that has none.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Arming {
    /// A fresh event says the record is in its state now: a dispatch, a
    /// create, a delete, a timer that found its record elsewhere, or an
    /// install that changed the record's spec. Or the boot walk, which finds
    /// the records a stopped process left without a row. A missing timer is
    /// armed.
    #[default]
    Any,
    /// A rehydration: only a timer the record has (armed from its row)
    /// follows its state. A record with neither waits
    /// ([`TimerFollow::WaitsForRow`]) for its next action or the next boot.
    FromRow,
}

/// How a reconcile treats the record: who its new timer fires as, the boot
/// gate it fires through, whether it may arm a record that has no timer, and
/// the deadline of the timer whose task follows the record, if one does.
#[derive(Clone, Default)]
pub(crate) struct Follow<'a> {
    /// The caller of the dispatch that reconciles, or whoever armed the row
    /// the boot dropped.
    pub(crate) armed_by: Option<&'a AgentContext>,
    /// The boot walk's fire gate.
    pub(crate) gate: Option<Arc<tokio::sync::Semaphore>>,
    pub(crate) arming: Arming,
    /// The deadline a timer's task woke for, when that task follows the
    /// record: it found the record elsewhere, or moved on by an action that
    /// does not reset the timer. See [`elapsed_bucket`].
    pub(crate) woke_for: Option<DateTime<Utc>>,
}

/// How late a timer a follow arms is, as
/// `temper_state_timeout_armed_on_hydration_total` counts it:
///
/// - `budgeted`: its deadline is ahead (`delay` is left);
/// - `on_time`: it is due, and the timer task that follows the record woke
///   for a deadline no later than this one, so a timer was waiting for it. A
///   record that took an action its timer does not reset on keeps the
///   timer's deadline, and the timer's own task arms it again at that
///   deadline, to fire at once;
/// - `overdue`: it is due, and its deadline passed with no timer waiting for
///   it, as when an actor hydrates or a process boots after it.
fn elapsed_bucket(
    delay: Duration,
    deadline: DateTime<Utc>,
    woke_for: Option<DateTime<Utc>>,
) -> &'static str {
    if !delay.is_zero() {
        "budgeted"
    } else if woke_for.is_some_and(|woke_for| woke_for <= deadline) {
        "on_time"
    } else {
        "overdue"
    }
}

impl ServerState {
    /// Make `entity_id`'s armed timer the one `current`'s state declares, or
    /// none. A timer already armed for that declaration is kept; any other is
    /// cancelled and its row removed. A timer armed here fires as `armed_by`,
    /// else as whoever armed the timer it replaces, else as the
    /// [`STATE_TIMER_SERVICE`] when its action does not bind a caller.
    pub(crate) async fn reconcile_state_timers(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        current: &EntityState,
        armed_by: Option<&AgentContext>,
    ) -> TimerFollow {
        let follow = Follow {
            armed_by,
            ..Follow::default()
        };
        self.follow_state_timer(tenant, entity_type, entity_id, current, follow)
            .await
    }

    /// [`Self::reconcile_state_timers`] as `follow` says.
    pub(super) async fn follow_state_timer(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        current: &EntityState,
        follow: Follow<'_>,
    ) -> TimerFollow {
        let Follow {
            armed_by,
            gate,
            arming,
            woke_for,
        } = follow;
        let declared = {
            let Ok(registry) = self.registry.read() else {
                return TimerFollow::Settled;
            };
            registry.get_spec(tenant, entity_type).and_then(|spec| {
                spec.automaton
                    .state_timeouts
                    .iter()
                    .find(|timeout| timeout.state == current.status)
                    .cloned()
            })
        };
        let now = sim_now();
        let Some(st) = declared else {
            self.cancel_state_timer(tenant, entity_type, entity_id, current.sequence_nr)
                .await;
            return TimerFollow::Settled;
        };
        let turn = self.state_timer_turn(tenant, entity_type, entity_id);
        let armed = self.armed_state_timer(tenant, entity_type, entity_id);
        // Rows that follow commits are the timers the journal armed: one the
        // record's events left for this state is the timer it has, even when
        // the process never armed it (the move's answer was lost, or the
        // actor rebuilt from its journal). Read even with a live arm: a
        // committed reset can have superseded it without post-dispatch work.
        // A withheld task releases its live arm but keeps its row, so an
        // install never replaces its setter with the service.
        let row = self
            .stored_state_timer_row(tenant, entity_type, entity_id)
            .await;
        let Ok(row) = row else {
            // An unread row may hold the committed arm: a timer built from the
            // record's fields now would replace its deadline, arguments and
            // caller. Leave the record's timer as it is.
            return TimerFollow::RetryRead;
        };
        // A reset can replace this arm at the same journal sequence while
        // the read is in flight. Its newer clock wins over this row snapshot.
        if self.state_timer_turn(tenant, entity_type, entity_id) != turn {
            return TimerFollow::Settled;
        }
        // The row committed with the record's events, so it is the current
        // state's clock when it names the state.
        let row = row.filter(|row| row.state == st.state && row.sequence_nr <= current.sequence_nr);
        let committed = row.as_ref().and_then(|row| {
            match super::state_timer_rows::armed_timer(
                tenant,
                row,
                &st.on_timeout,
                st.after_seconds,
            ) {
                Ok(timer) => Some(timer),
                Err(error) => {
                    tracing::warn!(tenant = %tenant, entity_type, entity_id, %error,
                        "committed state timer row not decodable");
                    None
                }
            }
        });
        // An unrecorded reset advances only the durable arm's clock, not
        // the journal's status clock. Read it before deciding a one-shot
        // already fired: the old fire may predate this replacement arm.
        let Some(started_at) = rearm_clock_start(
            &st,
            current,
            now,
            committed.as_ref().map(|timer| timer.started_at),
        ) else {
            self.cancel_state_timer(tenant, entity_type, entity_id, current.sequence_nr)
                .await;
            return TimerFollow::Settled;
        };
        if arming == Arming::FromRow && armed.is_none() && armed_by.is_none() && row.is_none() {
            return TimerFollow::WaitsForRow;
        }
        // A live timer of this same row knows who the row may not say: a boot
        // names the journal's setter in place of the row's service and keeps
        // the row. A row with another arm is newer than the live timer.
        let live_of_row = armed
            .as_ref()
            .filter(|armed| {
                committed
                    .as_ref()
                    .is_some_and(|row| row.arm_id == armed.arm_id)
            })
            .map(|armed| armed.agent_ctx.clone());
        let known = live_of_row
            .or_else(|| committed.as_ref().map(|timer| timer.agent_ctx.clone()))
            .or_else(|| armed_by.cloned())
            .or_else(|| armed.as_ref().map(|armed| armed.agent_ctx.clone()))
            .or_else(|| row.as_ref().and_then(super::state_timer_rows::row_armed_by));
        // Nothing names a setter, or only the service does and no timer of
        // this process holds it: the journal may name who the record follows.
        let known = if armed.is_none() && known.as_ref().is_none_or(is_state_timer_service) {
            match self.journal_setter(tenant, entity_type, entity_id).await {
                Ok(Some(setter)) => Some(setter),
                Ok(None) => known,
                Err(error) => {
                    tracing::warn!(tenant = %tenant, entity_type, entity_id, %error,
                        "record journal not readable; its timer's setter is not known yet");
                    return TimerFollow::RetryRead;
                }
            }
        } else {
            known
        };
        // Resolving the journal's setter above can also await storage.
        if self.state_timer_turn(tenant, entity_type, entity_id) != turn {
            return TimerFollow::Settled;
        }
        let identity = match known {
            Some(identity) => identity,
            None if self.action_binds_caller(tenant, entity_type, &st.on_timeout) => {
                if current.last_event.timer_needs_person.is_none() {
                    // A record that moved since this read has a newer follow
                    // of its own, which decides its timer.
                    self.say_timer_needs_person(
                        tenant,
                        entity_type,
                        entity_id,
                        &st.on_timeout,
                        current.sequence_nr,
                    )
                    .await;
                }
                return TimerFollow::WaitsForCaller;
            }
            None => AgentContext::for_service(STATE_TIMER_SERVICE),
        };
        let mut timer = declared_timer(
            tenant,
            entity_type,
            &st,
            current,
            &ReadDefaults::for_entity(self, tenant, entity_type),
            &identity,
            started_at,
        );
        let reservation = if let Some(committed) = committed {
            // The committed arm's identity, unless the journal named its setter
            // in place of the service.
            let agent_ctx = std::mem::take(&mut timer.agent_ctx);
            timer = committed;
            timer.agent_ctx = agent_ctx;
            // Keep the durable arm's clock, payload, caller and row identity,
            // but fence its replacement at the record we just reconciled.
            // Non-resetting actions advance the journal without a new arm.
            timer.sequence_nr = current.sequence_nr;
            Reservation::Committed
        } else {
            Reservation::Missing
        };
        timer.boot_gate = gate.or_else(|| armed.and_then(|armed| armed.boot_gate));
        let delay = remaining_budget(timer.started_at, st.after_seconds, now);
        let bucket = elapsed_bucket(
            delay,
            deadline(timer.started_at, st.after_seconds),
            woke_for,
        );
        if self.start_armed_timer(timer, reservation).await {
            crate::runtime_metrics::record_state_timeout_armed_on_hydration(
                tenant.as_str(),
                entity_type,
                &st.state,
                bucket,
            );
        }
        TimerFollow::Settled
    }

    /// [`Self::follow_state_timer`] for a task that owns the record's timer
    /// (its timer's task, or its rehydration's): a timer row that cannot be
    /// read is read again, after a wait that doubles from
    /// [`FOLLOW_RETRY_FIRST`] to [`FOLLOW_RETRY_MAX`] as the walks' does.
    /// Guessing would replace the committed arm's deadline, arguments and
    /// caller; giving up would leave the record with no timer. It stops once
    /// another arm or cancellation in this process takes the record over.
    pub(super) async fn follow_owned_state_timer(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
        current: &EntityState,
        follow: Follow<'_>,
    ) -> TimerFollow {
        let owned = self.state_timer_turn(tenant, entity_type, entity_id);
        let mut wait = FOLLOW_RETRY_FIRST;
        loop {
            let followed = self
                .follow_state_timer(tenant, entity_type, entity_id, current, follow.clone())
                .await;
            if followed != TimerFollow::RetryRead {
                return followed;
            }
            tracing::warn!(tenant = %tenant, entity_type, entity_id,
                retry_in_secs = wait.as_secs(),
                "state timer row not readable; the record's timer waits for it");
            tokio::time::sleep(wait).await; // determinism-ok: retry wait on the runtime clock
            if self.state_timer_turn(tenant, entity_type, entity_id) != owned {
                return followed;
            }
            wait = (wait * 2).min(FOLLOW_RETRY_MAX);
        }
    }

    /// Whether `action` binds a parameter from its caller's identity, which a
    /// service caller cannot give (`resolve_authenticated_params`).
    fn action_binds_caller(&self, tenant: &TenantId, entity_type: &str, action: &str) -> bool {
        self.transition_table_for_dispatch(tenant, entity_type)
            .is_ok_and(|table| {
                table
                    .action_contracts
                    .get(action)
                    .is_some_and(|contract| !contract.param_sources.is_empty())
            })
    }

    /// Whether `entity_type`'s spec declares any state timeout.
    pub(super) fn declares_state_timeouts(&self, tenant: &TenantId, entity_type: &str) -> bool {
        self.registry.read().ok().is_some_and(|registry| {
            registry
                .get_spec(tenant, entity_type)
                .is_some_and(|spec| !spec.automaton.state_timeouts.is_empty())
        })
    }

    /// The hook an entity actor runs when it starts from stored events: it
    /// makes the record's timer, if it has one, follow the state it
    /// rehydrated into ([`Arming::FromRow`]). None for a type that declares no
    /// timeout and a record with no timer armed.
    pub(crate) fn state_timer_rehydrate_hook(
        &self,
        tenant: &TenantId,
        entity_type: &str,
        entity_id: &str,
    ) -> Option<RehydrateHook> {
        if !self.declares_state_timeouts(tenant, entity_type)
            && !self.has_armed_state_timer(tenant, entity_type, entity_id)
        {
            return None;
        }
        let server = self.clone();
        let (tenant, entity_type, entity_id) = (
            tenant.clone(),
            entity_type.to_string(),
            entity_id.to_string(),
        );
        Some(Arc::new(move |rehydrated: &EntityState| {
            let (server, rehydrated) = (server.clone(), rehydrated.clone());
            let (tenant, entity_type, entity_id) =
                (tenant.clone(), entity_type.clone(), entity_id.clone());
            // determinism-ok: background reconcile, as timers are spawned; under
            // simulation it runs on the single-threaded test runtime.
            tokio::spawn(async move {
                let follow = Follow {
                    arming: Arming::FromRow,
                    ..Follow::default()
                };
                server
                    .follow_owned_state_timer(
                        &tenant,
                        &entity_type,
                        &entity_id,
                        &rehydrated,
                        follow,
                    )
                    .await;
            });
        }))
    }
}
