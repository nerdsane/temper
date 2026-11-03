//! That a record's state timer stopped because nothing names its caller
//! (`state::dispatch::state_timer_stop`).
//!
//! The event is the kernel's, like `SpawnRefused`: it changes no status, runs
//! no spec effects and is replayed through [`LastEvent`], the same as live.
//!
//! [`LastEvent`]: super::LastEvent

use temper_runtime::persistence::PersistenceError;
use temper_runtime::scheduler::sim_now;

use super::actor::EntityActor;
use super::types::{EntityEvent, EntityState, MAX_EVENTS_SINCE_SNAPSHOT};

/// Journal event type. Reserved: no spec action may use it.
pub(crate) const TIMER_NEEDS_PERSON_EVENT: &str = "TimerNeedsPerson";

/// Attempts allowed after the first, matching field updates.
const MAX_RETRIES: u32 = 2;

/// The refused action a [`TIMER_NEEDS_PERSON_EVENT`] names.
pub(super) fn refused_action(event: &EntityEvent) -> Option<String> {
    event.params.get("action")?.as_str().map(str::to_owned)
}

fn event_of(state: &EntityState, action: &str) -> EntityEvent {
    EntityEvent {
        action: TIMER_NEEDS_PERSON_EVENT.to_owned(),
        from_status: state.status.clone(),
        to_status: state.status.clone(),
        timestamp: sim_now(),
        params: serde_json::json!({ "action": action }),
        idempotency_key: None,
        outcome: None,
        origin: super::EventOrigin::internal(),
        kind: temper_runtime::persistence::EventKind::Transition,
    }
}

/// What a [`commit`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    /// The record says so durably.
    Said,
    /// The record moved past the sequence the stop was decided at, so the
    /// stop was not recorded: whoever moved it may name a caller.
    Moved,
}

/// Journal that the record's timer stopped for `action`, decided with the
/// record at journal sequence `checked`, unless it says so already, the record
/// moved since, or it cannot take it. On `Err` it is unchanged.
pub(super) async fn commit(
    actor: &EntityActor,
    state: &mut EntityState,
    action: &str,
    checked: u64,
) -> Result<Stop, String> {
    let mut attempt: u32 = 0;
    loop {
        if state.last_event.timer_needs_person.is_some() {
            return Ok(Stop::Said);
        }
        if state.sequence_nr != checked {
            return Ok(Stop::Moved);
        }
        if state.status == "Deleted" {
            return Err("the record was deleted".to_owned());
        }
        // The budget that keeps the replay tail bounded holds for kernel events too.
        if state.events_since_snapshot >= MAX_EVENTS_SINCE_SNAPSHOT {
            return Err(format!(
                "Event budget exhausted ({MAX_EVENTS_SINCE_SNAPSHOT} max since snapshot)"
            ));
        }
        let event = event_of(state, action);
        let (Some(store), Some(backend)) = (actor.event_journal.as_ref(), actor.event_backend)
        else {
            state.push_event_bounded(event);
            return Ok(Stop::Said);
        };
        match actor
            .persist_event(
                store,
                backend,
                state,
                &event,
                &super::scheduled_rows::ScheduledRows::default(),
                None,
            )
            .await
        {
            Ok(_) => {
                state.push_event_bounded(event);
                let persistence_id = actor.persistence_id();
                if let Err(e) = actor
                    .now
                    .during(
                        super::watchdog::Doing::Snapshotting,
                        EntityActor::maybe_save_snapshot(
                            store,
                            actor.snapshot_queue.as_ref(),
                            &persistence_id,
                            state,
                        ),
                    )
                    .await
                {
                    tracing::warn!(entity = %state.entity_id, seq = state.sequence_nr, error = %e,
                        "failed to persist snapshot");
                }
                return Ok(Stop::Said);
            }
            Err(PersistenceError::ConcurrencyViolation { actual, .. }) if attempt < MAX_RETRIES => {
                attempt += 1;
                super::field_updates::catch_up(
                    actor,
                    state,
                    attempt,
                    actual,
                    TIMER_NEEDS_PERSON_EVENT,
                )
                .await?;
            }
            Err(e) => return Err(super::actor::persistence_failure(&e)),
        }
    }
}
