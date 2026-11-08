//! A move that leaves its record exactly as it found it writes nothing.
//!
//! A timer's poll with no effects, the reaction hop it sets off only to run a
//! module, a module's answer whose facts the record already holds, an agent's
//! look: each wrote an event, a trajectory row and the record's rows, and moved
//! the snapshot cadence on, so a record polled every few seconds grew its
//! journal without ever changing. One app's database reached 3.35 GB that way,
//! too big to restore within the upgrade limit; 61% of its last 100,000 events
//! were such moves. Such a move now writes no event, row, trajectory entry or
//! snapshot step, and is not pushed into the record's journal. Its triggers
//! still run, from the answer's state, and a timer its action resets re-arms
//! from the move's own time ([`EntityResponse::unrecorded_at`]).
//!
//! A timer's own fire goes on under the row it fired from, so it writes no row
//! either; a reset by anyone else writes one, so a restart keeps it
//! (`arm_state_timeouts_if_needed`).

use super::EventVia;
use super::actor::EntityActor;
use super::effects::ProcessResult;
use super::scheduled_rows::ScheduledRows;
use super::types::{EntityEvent, EntityResponse, EntityState};

/// Whether the record already holds everything the move's event would say.
///
/// A machine's move (`Timer`, `Reaction`) carries nothing of its own. A move
/// someone asked for (`Request`, an HTTP or MCP call by a person or an agent)
/// can carry params the record does not keep, such as a transient field, and
/// then its event is the only place they exist, so it is written. A move whose
/// origin is unknown is written too.
pub(super) fn record_holds_what_it_says(event: &EntityEvent, after: &EntityState) -> bool {
    match event.origin.via {
        Some(EventVia::Timer | EventVia::Reaction) => true,
        Some(EventVia::Request) => event.params.as_object().is_none_or(|params| {
            params
                .iter()
                .all(|(name, value)| after.fields.get(name) == Some(value))
        }),
        None => false,
    }
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
    // (the status clock's entry for its action), that a move naming someone
    // came after a timer stopped for want of one (`timer_needs_person`), and
    // that someone other than the last to move asked for this one. A record says
    // who moved it last on every read, and its later timers run as the newest
    // move that names someone (`journal_setter`), so that first move changes
    // the record. The same caller asking again changes nothing.
    let clears_timer_stop =
        before.last_event.timer_needs_person.is_some() && event.origin.by.is_some();
    let names_someone_new = event.origin.via == Some(EventVia::Request)
        && event.origin.by.is_some()
        && event.origin.by != before.last_event.last_event_by;
    record_kept && !writes_beside && !fires_timer_once && !clears_timer_stop && !names_someone_new
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::entity_actor::EventOrigin;

    fn event(via: Option<EventVia>, params: serde_json::Value) -> EntityEvent {
        EntityEvent {
            action: "Look".to_string(),
            from_status: "Open".to_string(),
            to_status: "Open".to_string(),
            timestamp: chrono::DateTime::UNIX_EPOCH,
            params,
            origin: EventOrigin {
                via,
                ..Default::default()
            },
            idempotency_key: None,
            kind: Default::default(),
            outcome: None,
        }
    }

    fn record(fields: serde_json::Value) -> EntityState {
        EntityState {
            entity_type: "Thing".to_string(),
            entity_id: "t".to_string(),
            status: "Open".to_string(),
            item_count: 0,
            counters: Default::default(),
            booleans: Default::default(),
            lists: Default::default(),
            fields,
            events: Default::default(),
            total_event_count: 1,
            events_since_snapshot: 1,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 1,
            processed_idempotency_keys: Default::default(),
            last_event: Default::default(),
            status_clock: Default::default(),
            spawn_reservations: Default::default(),
        }
    }

    #[test]
    fn a_request_is_written_only_for_what_the_record_does_not_hold() {
        let kept = record(json!({ "note": "same" }));
        let via = Some(EventVia::Request);
        assert!(record_holds_what_it_says(&event(via, json!({})), &kept));
        assert!(record_holds_what_it_says(
            &event(via, json!({ "note": "same" })),
            &kept
        ));
        assert!(
            !record_holds_what_it_says(&event(via, json!({ "PackBytes": "abc" })), &kept),
            "a param the record does not keep exists only in the event"
        );
    }

    #[test]
    fn a_machine_says_nothing_of_its_own_and_an_unknown_origin_is_written() {
        let kept = record(json!({}));
        let params = json!({ "asked": "x" });
        for via in [EventVia::Timer, EventVia::Reaction] {
            assert!(record_holds_what_it_says(
                &event(Some(via), params.clone()),
                &kept
            ));
        }
        assert!(!record_holds_what_it_says(&event(None, json!({})), &kept));
    }
}
