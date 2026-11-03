//! The durable transaction behind OData PATCH/PUT (ARN-189, ADR-0157).
//!
//! Field updates bypass the spec's action vocabulary — no guards, no effects,
//! no transition — but they still change entity state, so they have to be
//! journaled or they vanish on eviction and restart. That makes this a small
//! transaction with the same obligations as the `Action` arm: refuse before
//! mutating, never acknowledge what was not persisted, and converge with what
//! replay will rebuild.
//!
//! It lives beside the actor rather than inside the message match because it is
//! ~250 lines of policy, and because the caller's only job is to turn the
//! outcome into a reply.

use serde_json::Value;
use temper_runtime::persistence::PersistenceError;
use temper_runtime::scheduler::sim_now;

use super::actor::{EntityActor, ReplayLogging, ReplayPolicy};
use super::effects;
use super::types::{EntityEvent, EntityState, MAX_EVENTS_SINCE_SNAPSHOT};

/// Attempts allowed after the first, matching the `Action` arm's ADR-0046 budget.
const MAX_RETRIES: u32 = 2;

/// Why a field update is not in the journal. `state` has been restored (a
/// refusal never leaves a partial write), except on `RecordChanged`, where it
/// is caught up.
pub(super) enum NotUpdated {
    /// The update was decided on a state the journal had already moved past.
    /// The record is caught up, and the route decides the update again on it.
    RecordChanged,
    /// Refused or failed before the store was asked; the caller replies with
    /// the reason verbatim.
    Failed(String),
    /// The store did not take the update's event.
    Store(PersistenceError),
}

/// Apply a field update and make it durable, or refuse it.
///
/// `Ok(())` means the update is in the journal. An error means it is not, and
/// `state` has been restored (a refusal never leaves a partial write) or, on
/// [`NotUpdated::RecordChanged`], caught up with the journal.
pub(super) async fn commit_field_update(
    actor: &EntityActor,
    state: &mut EntityState,
    fields: Value,
    replace: bool,
    has_precondition: bool,
    caller: Option<&crate::request_context::AgentContext>,
) -> Result<(), NotUpdated> {
    if actor
        .table
        .read()
        .expect("table lock poisoned")
        .strict_action_params
    {
        return Err(NotUpdated::Failed(
            "Strict entities require a declared action for field changes".to_owned(),
        ));
    }
    if state.status == "Deleted" {
        return Err(NotUpdated::Failed(
            "cannot update fields after entity deletion".to_string(),
        ));
    }
    // `parse_json_body_or_400` accepts any valid JSON, so a `PUT` body of
    // `[1,2,3]` reaches here. With `replace`, `apply_field_update` would set
    // `fields` to the array and then fail to restore `Id`/`Status` — there is no
    // object to insert into — and the append would co-commit zero key and zero
    // vector rows, purging the entity's index. Before field updates were
    // journaled that corruption was in-memory and healed on restart; persisting
    // it would make it permanent.
    if !fields.is_object() {
        return Err(NotUpdated::Failed(
            "entity field update must be a JSON object".to_string(),
        ));
    }
    // The same budget that gates spec actions. Field updates append journal
    // events too, so ungated they could grow the snapshot replay tail past
    // `MAX_EVENTS_SINCE_SNAPSHOT` while the snapshot path is stalled, after which
    // the entity can never rehydrate. Rejected before mutating.
    if let Some(reason) = budget_refusal(actor, state, replace) {
        return Err(NotUpdated::Failed(reason));
    }

    // Sanitize once and use the same value for state and journal.
    // `apply_field_update` sanitizes internally as well — it must, so that
    // replaying an event written before this guard still lands on clean state —
    // but the event written now must not carry a caller's forged `has_spec` or
    // `ctx_owner_status` into the journal in the first place.
    let fields = effects::sanitize_action_params(&fields).into_owned();

    let action = if replace {
        effects::FIELDS_REPLACED_EVENT
    } else {
        effects::FIELDS_UPDATED_EVENT
    };

    // Apply speculatively so the append co-commits key/vector index rows derived
    // from the NEW fields, then journal fail-closed: an update that is not
    // durable must not be acknowledged. Rolled back on failure.
    let mut previous_fields = state.fields.clone();
    // Bind the result: `debug_assert!` does not evaluate its argument in release
    // builds, so asserting the call directly would skip the update in production.
    let applied = effects::apply_field_update(state, &fields, replace);
    debug_assert!(
        applied,
        "object-ness was checked above; the apply cannot decline"
    );
    let mut event = field_event(action, state, &fields);
    event.origin = super::EventOrigin::from_caller(caller);

    let (Some(store), Some(backend)) = (actor.event_journal.as_ref(), actor.event_backend) else {
        // The same event metadata is visible without a journal.
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
            // A preconditioned update is a compare-and-set: the caller authorized
            // this write against one exact state digest. A conflict is proof the
            // journal held state the actor's memory did not, so replaying and
            // re-applying would commit against state the caller never saw and
            // Cedar never evaluated. `entity_ops` already caps preconditioned asks
            // at a single attempt for that reason; retrying here would reintroduce
            // one layer down the retry the layer above forbids. The record is
            // caught up instead, so the route decides the update again on it.
            Err(PersistenceError::ConcurrencyViolation { actual, .. }) if has_precondition => {
                if let Err(reason) = catch_up(actor, state, 1, actual, action).await {
                    state.fields = previous_fields;
                    return Err(NotUpdated::Failed(reason));
                }
                return Err(NotUpdated::RecordChanged);
            }
            Err(PersistenceError::ConcurrencyViolation { actual, .. }) if attempt < MAX_RETRIES => {
                attempt += 1;
                match catch_up(actor, state, attempt, actual, action).await {
                    Ok(()) => {}
                    Err(reason) => {
                        state.fields = previous_fields;
                        return Err(NotUpdated::Failed(reason));
                    }
                }
                if let Some(reason) = post_replay_refusal(actor, state, replace) {
                    return Err(NotUpdated::Failed(reason));
                }
                // Re-apply onto the caught-up state and rebuild the event against
                // its (possibly new) status.
                previous_fields = state.fields.clone();
                let applied = effects::apply_field_update(state, &fields, replace);
                debug_assert!(
                    applied,
                    "object-ness was checked above; the apply cannot decline"
                );
                event = field_event(action, state, &fields);
                event.origin = super::EventOrigin::from_caller(caller);
            }
            Err(e) => {
                state.fields = previous_fields;
                return Err(NotUpdated::Store(e));
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
        tracing::warn!(
            entity = %state.entity_id,
            seq = state.sequence_nr,
            error = %e,
            "failed to persist snapshot"
        );
    }

    Ok(())
}

/// The refusal an update gets when the state it was authorized against moved,
/// whether the actor's entry check or a conflict on its append found it. The
/// route then decides the update again on the record as it is now.
pub(super) const STALE_AUTHORIZATION: &str =
    "field update authorization became stale; retry against current state";

fn field_event(action: &str, state: &EntityState, fields: &Value) -> EntityEvent {
    EntityEvent {
        action: action.to_string(),
        from_status: state.status.clone(),
        to_status: state.status.clone(),
        timestamp: sim_now(),
        params: fields.clone(),
        idempotency_key: None,
        origin: crate::entity_actor::EventOrigin::internal(),
        kind: temper_runtime::persistence::EventKind::Transition,
        outcome: None,
    }
}

fn budget_refusal(actor: &EntityActor, state: &EntityState, replace: bool) -> Option<String> {
    if state.events_since_snapshot < MAX_EVENTS_SINCE_SNAPSHOT {
        return None;
    }
    let workspace_id = super::actor::event_budget_workspace_id(state);
    crate::event_budget_metrics::record_exhausted(
        &actor.tenant,
        &state.entity_type,
        &state.entity_id,
        &workspace_id,
    );
    tracing::warn!(
        tenant = %actor.tenant,
        entity_type = %state.entity_type,
        entity_id = %state.entity_id,
        workspace_id = %workspace_id,
        status = %state.status,
        replace,
        events_since_snapshot = state.events_since_snapshot,
        total_event_count = state.total_event_count,
        max_events_since_snapshot = MAX_EVENTS_SINCE_SNAPSHOT,
        "Event budget exhausted (field update rejected)"
    );
    Some(format!(
        "Event budget exhausted ({MAX_EVENTS_SINCE_SNAPSHOT} max since snapshot)"
    ))
}

/// Rebuild `state` from the journal after a sequence conflict. A field update
/// and a delete that met one catch up through here before they answer.
///
/// Rebuilds from a *fresh* initial state, exactly as
/// `recover_entity_state_from_store` does. `replay_events` applies onto whatever
/// state it is handed and never resets it, so replaying onto the live state
/// re-applies every event on top of its own effects: the events deque grows,
/// `total_event_count` / `events_since_snapshot` climb, and non-idempotent
/// effects (counter increments) fire twice. That corruption would be returned to
/// the caller, upserted into the query projection, and made durable by the next
/// snapshot. Rolling back `fields` alone cannot help — those other fields were
/// never part of the speculative update.
pub(super) async fn catch_up(
    actor: &EntityActor,
    state: &mut EntityState,
    attempt: u32,
    actual: u64,
    action: &str,
) -> Result<(), String> {
    // Back off first, or against a live concurrent writer the whole budget burns
    // in microseconds and the retry buys nothing.
    actor
        .now
        .during(
            super::watchdog::Doing::RetryPause,
            super::actor::sleep_persistence_retry(std::time::Duration::from_millis(
                if attempt == 1 { 10 } else { 50 },
            )),
        )
        .await;
    tracing::warn!(
        entity = %state.entity_id,
        action = %action,
        actual_seq = actual,
        attempt,
        "write hit an optimistic-concurrency violation; catching up with the journal"
    );

    let (Some(store), Some(backend)) = (actor.event_journal.as_ref(), actor.event_backend) else {
        return Err("persistence unavailable during conflict recovery".to_string());
    };
    let table = actor.table.read().expect("table lock poisoned").clone();
    let mut caught_up = EntityActor::build_initial_state(
        &actor.entity_type,
        &state.entity_id,
        &table,
        &actor.initial_fields,
    );
    // A replay failure is returned as a refusal rather than propagated: bailing
    // out of the message handler would leave the caller's `ask` unanswered until
    // it times out, with the entity mid-rollback.
    actor
        .now
        .during(
            super::watchdog::Doing::Replaying,
            EntityActor::replay_events(
                &table,
                store,
                backend,
                &mut caught_up,
                &actor.tenant,
                actor.blob_store.as_ref(),
                (ReplayPolicy::StrictSnapshot, ReplayLogging::Actor),
            ),
        )
        .await
        .map_err(|e| format!("conflict recovery replay failed: {e}"))?;
    debug_assert!(
        caught_up.sequence_nr >= actual,
        "POSTCONDITION: field-update replay under-reached the authoritative sequence \
         (sequence_nr={} < actual={actual})",
        caught_up.sequence_nr
    );
    *state = caught_up;
    Ok(())
}

/// Re-run the refusals that were checked before the first attempt. The race may
/// have deleted the entity or spent the budget, and both were only ever checked
/// against the actor's pre-conflict memory.
fn post_replay_refusal(actor: &EntityActor, state: &EntityState, replace: bool) -> Option<String> {
    if state.status == "Deleted" {
        return Some("cannot update fields after entity deletion".to_string());
    }
    budget_refusal(actor, state, replace)
}
