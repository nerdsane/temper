//! A move a machine made that leaves its record exactly as it found it
//! writes nothing.
//!
//! A timer's poll with no effects, the reaction hop it sets off only to run a
//! module, a module's answer whose facts the record already holds: each wrote
//! an event, a trajectory row and the record's rows, and moved the snapshot
//! cadence on, so a record polled every few seconds grew its journal without
//! ever changing. One app's database reached 3.35 GB that way, too big to
//! restore within the upgrade limit; 61% of its last 100,000 events were such
//! moves. Such a move now writes no event, row, trajectory entry or snapshot
//! step, and is not pushed into the record's journal. Its triggers still run,
//! from the answer's state, and a timer its action resets re-arms from the
//! move's own time ([`EntityResponse::unrecorded_at`]), writing its own row in
//! place of the one the event would have carried.

use super::actor::EntityActor;
use super::effects::ProcessResult;
use super::scheduled_rows::ScheduledRows;
use super::types::{EntityEvent, EntityResponse, EntityState};
use super::{EventOrigin, EventVia};

/// Whether a machine made the move: a state timer or scheduled action
/// (`Timer`), or a reaction, an integration's or module's callback, or
/// another kernel-owned move (`Reaction`).
///
/// A move a person or an agent asked for directly (an HTTP or MCP request,
/// `Request`) is recorded even when it changes nothing: it is an intentional
/// act, bounded by human speed, and who-did-what history is for it. A move
/// whose origin is unknown is recorded too.
pub(super) fn made_by_machine(origin: &EventOrigin) -> bool {
    matches!(origin.via, Some(EventVia::Timer | EventVia::Reaction))
}

/// Whether a successful move leaves its record exactly as it found it, and
/// needs its event for nothing else, so writing it would record nothing.
///
/// `before` is the record when the move started and `after` the record the
/// move made; `result` and `event` are the move's; `scheduled` is the
/// scheduled-action rows its commit would change; `fires_timer_once` says the
/// move is the fire of a state timer that does not reset on its own action.
pub(super) fn leaves_record_unchanged(
    before: &EntityState,
    after: &EntityState,
    result: &ProcessResult,
    event: &EntityEvent,
    scheduled: &ScheduledRows,
    fires_timer_once: bool,
) -> bool {
    // What a move can change, as `effects::MoveStart` copies it: the status,
    // counters (`item_count` among them), booleans, lists, the fields with the
    // action's params written into them, and spawn reservations. The record's
    // key and vector rows are read from its fields, so they stay as they are.
    let record_kept = before.status == after.status
        && before.item_count == after.item_count
        && before.counters == after.counters
        && before.booleans == after.booleans
        && before.lists == after.lists
        && before.fields == after.fields
        && before.spawn_reservations == after.spawn_reservations;
    // What the move writes beside the record, in its event's commit or after
    // it: children, scheduled actions and their rows (a scheduled action's
    // fire removes its row with its event), and field-overflow blobs.
    let writes_beside = !result.spawn_requests.is_empty()
        || !result.scheduled_actions.is_empty()
        || !result.overflow_blobs.is_empty()
        || !scheduled.armed.is_empty()
        || scheduled.fired.is_some();
    // What only its event would say: that a timer which fires once has fired
    // (the status clock's entry for its action), and that a move naming
    // someone came after a timer stopped for want of one (`timer_needs_person`).
    let clears_timer_stop =
        before.last_event.timer_needs_person.is_some() && event.origin.by.is_some();
    record_kept && !writes_beside && !fires_timer_once && !clears_timer_stop
}

impl EntityActor {
    /// The answer to a move that writes nothing: the record as it was, the
    /// move's custom effects for its triggers, and the move's own time.
    pub(super) fn answer_unrecorded(
        &self,
        state: &EntityState,
        result: ProcessResult,
        event: &EntityEvent,
    ) -> EntityResponse {
        debug_assert!(
            result.success && result.spawn_requests.is_empty(),
            "PRECONDITION: only a successful move that spawns nothing goes unrecorded"
        );
        debug_assert_eq!(
            event.from_status, state.status,
            "PRECONDITION: an unrecorded move keeps its record's status"
        );
        crate::runtime_metrics::record_unrecorded_move(
            &self.tenant,
            &state.entity_type,
            &event.action,
        );
        tracing::debug!(
            tenant = %self.tenant,
            entity_type = %state.entity_type,
            entity_id = %state.entity_id,
            action = %event.action,
            status = %state.status,
            via = ?event.origin.via,
            seq = state.sequence_nr,
            "move left its record unchanged; nothing written"
        );
        EntityResponse {
            success: true,
            refusal: None,
            state: state.clone(),
            error: None,
            custom_effects: result.custom_effects,
            scheduled_actions: Vec::new(),
            spawn_requests: Vec::new(),
            spec_governed: true,
            refused: false,
            answered_action: None,
            trigger_failures: Vec::new(),
            store_unavailable: false,
            unrecorded_at: Some(event.timestamp),
        }
    }
}
