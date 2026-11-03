//! A spawn whose child the kernel could not create, recorded on its parent.
//!
//! A `spawn(...)` effect writes the child's id into the parent's `store_id_in`
//! field as part of the parent's transition, and the kernel creates the child
//! after that transition commits (ADR-0015). That id is a reservation, not
//! proof the child exists (ADR-0156). When the create is refused, the kernel
//! journals a `SpawnRefused` event on the parent: it names the child and the
//! reason, and puts the field back to its initial value if it still names the
//! refused child, the original reservation still owns the field, and that
//! child is known not to exist. A later spawn keeps its own reservation,
//! even for the same child id. The id also stays when the child exists
//! anyway or could not be looked up.
//!
//! The event is the kernel's, like `FieldsUpdated`: it changes no status, runs
//! no spec effects and is replayed by [`apply`], the same function the live
//! path uses.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use temper_runtime::persistence::PersistenceError;
use temper_runtime::scheduler::sim_now;

use super::actor::EntityActor;
use super::types::{EntityEvent, EntityState, MAX_EVENTS_SINCE_SNAPSHOT};

/// Journal event type for a refused spawn. Reserved: no spec action may use it.
pub(crate) const SPAWN_REFUSED_EVENT: &str = "SpawnRefused";

/// Attempts allowed after the first, matching field updates.
const MAX_RETRIES: u32 = 2;

/// Why a spawn's child was not created, as the spawn dispatcher saw it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpawnRefusal {
    /// The original reservation, distinct from the child id it reserved.
    #[serde(default)]
    pub reservation_id: String,
    /// The child's entity type.
    pub child_type: String,
    /// The id the parent's transition reserved for the child.
    pub child_id: String,
    /// The parent field the spawn stored the id in, if any.
    pub field: Option<String>,
    /// What the kernel found when it looked the child up.
    pub child: ChildLookup,
    /// The refusal, as the step that refused it reported it.
    pub error: String,
}

/// Whether the refused child exists, as the kernel's durable lookup found it.
/// Only [`ChildLookup::Absent`] lets the refusal clear the parent's field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildLookup {
    /// No child with that id, resident or in the journal.
    Absent,
    /// A child with that id exists anyway, created by an earlier spawn or
    /// write. Its id is kept.
    Exists,
    /// The lookup failed. The child may exist, so its id is kept: clearing it
    /// on a guess could cut the parent off from a real child.
    Unreadable,
}

/// The event's params: the refusal, plus the field it cleared and the value it
/// restored, so replay does not have to decide again.
fn event_params(
    refusal: &SpawnRefusal,
    state: &EntityState,
    initial: &BTreeMap<String, Value>,
) -> Value {
    let owns_field = refusal.field.as_ref().is_some_and(|field| {
        state.fields.get(field).and_then(Value::as_str) == Some(refusal.child_id.as_str())
            && state.spawn_reservations.get(field) == Some(&refusal.reservation_id)
    });
    let cleared = (refusal.child == ChildLookup::Absent && owns_field)
        .then(|| refusal.field.clone())
        .flatten();
    let restored = cleared
        .as_ref()
        .map(|field| initial.get(field).cloned().unwrap_or(Value::Null));
    serde_json::json!({
        "reservation_id": refusal.reservation_id,
        "child_type": refusal.child_type,
        "child_id": refusal.child_id,
        "error": refusal.error,
        "child": refusal.child,
        "cleared_field": cleared,
        "restored_value": restored,
    })
}

/// Apply a `SpawnRefused` event's params to the parent. The single function
/// both the live handler and journal replay use.
pub(crate) fn apply(state: &mut EntityState, params: &Value) {
    let Some(field) = params.get("cleared_field").and_then(Value::as_str) else {
        return;
    };
    let restored = params.get("restored_value").cloned().unwrap_or(Value::Null);
    if let Some(fields) = state.fields.as_object_mut() {
        fields.insert(field.to_owned(), restored);
        state.spawn_reservations.remove(field);
    }
}

fn refusal_event(state: &EntityState, params: Value) -> EntityEvent {
    EntityEvent {
        action: SPAWN_REFUSED_EVENT.to_owned(),
        from_status: state.status.clone(),
        to_status: state.status.clone(),
        timestamp: sim_now(),
        params,
        idempotency_key: None,
        outcome: None,
        origin: crate::entity_actor::EventOrigin::internal(),
        kind: temper_runtime::persistence::EventKind::Transition,
    }
}

/// Journal the refusal on the parent and apply it, or say why it was not.
///
/// `Ok(())` means the event is durable. On `Err` the parent is unchanged.
pub(super) async fn commit(
    actor: &EntityActor,
    state: &mut EntityState,
    refusal: &SpawnRefusal,
) -> Result<(), String> {
    debug_assert!(
        !refusal.child_id.is_empty(),
        "PRECONDITION: a refused spawn names the child id it reserved"
    );
    if state.status == "Deleted" {
        return Err("the parent was deleted before its spawn was refused".to_owned());
    }
    // The budget that keeps the replay tail bounded holds for kernel events too.
    if state.events_since_snapshot >= MAX_EVENTS_SINCE_SNAPSHOT {
        return Err(format!(
            "Event budget exhausted ({MAX_EVENTS_SINCE_SNAPSHOT} max since snapshot)"
        ));
    }
    // Declared defaults exist even when the parent has no input contracts
    // and its initial stored fields therefore omit them.
    let initial = actor
        .table
        .read()
        .expect("table lock poisoned")
        .initial_values
        .fields
        .clone();
    let mut previous_fields = state.fields.clone();
    let mut previous_reservations = state.spawn_reservations.clone();
    let mut event = refusal_event(state, event_params(refusal, state, &initial));
    apply(state, &event.params);

    let (Some(store), Some(backend)) = (actor.event_journal.as_ref(), actor.event_backend) else {
        state.push_event_bounded(event);
        return Ok(());
    };
    let mut attempt: u32 = 0;
    loop {
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
            Ok(_) => break,
            Err(PersistenceError::ConcurrencyViolation { actual, .. }) if attempt < MAX_RETRIES => {
                attempt += 1;
                state.fields = previous_fields.clone();
                state.spawn_reservations = previous_reservations.clone();
                super::field_updates::catch_up(actor, state, attempt, actual, SPAWN_REFUSED_EVENT)
                    .await?;
                if state.status == "Deleted" {
                    return Err("the parent was deleted before its spawn was refused".to_owned());
                }
                // Another writer may have spent the replay budget while this
                // append conflicted. Refuse before rebuilding or applying it.
                if state.events_since_snapshot >= MAX_EVENTS_SINCE_SNAPSHOT {
                    return Err(format!(
                        "Event budget exhausted ({MAX_EVENTS_SINCE_SNAPSHOT} max since snapshot)"
                    ));
                }
                // Decide again on the caught-up parent: a newer spawn may have
                // replaced the reservation in between, even with the same id.
                previous_fields = state.fields.clone();
                previous_reservations = state.spawn_reservations.clone();
                event = refusal_event(state, event_params(refusal, state, &initial));
                apply(state, &event.params);
            }
            Err(e) => {
                state.fields = previous_fields;
                state.spawn_reservations = previous_reservations;
                return Err(super::actor::persistence_failure(&e));
            }
        }
    }
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
    Ok(())
}

#[cfg(test)]
#[path = "spawn_refusal_test.rs"]
mod tests;
