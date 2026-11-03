//! A state timer's fire applies only at the journal sequence its timer was
//! armed at.
//!
//! Two writers can hold one arming of a timer: the old and the new process
//! during a deploy (the old one armed it in memory, the new one again from
//! its row), or a fire sent again after its answer was lost. Both fire at the
//! sequence the arming was read at. The first commit moves the journal past
//! it. The other fire then finds the record moved, at its first evaluation or
//! after the ADR-0046 retry replays the journal, and is dropped instead of
//! being evaluated again against the moved record. The timer's task then
//! makes the record's timer follow its current state
//! (`ServerState::follow_moved_record`), which arms the next timer at the
//! record's current sequence.

use super::types::EntityState;
use crate::request_context::AgentContext;

/// The journal sequence the state timer was armed at, when `caller` fires
/// that timer's `action` on this record.
pub(super) fn armed_seq(
    caller: Option<&AgentContext>,
    tenant: &str,
    state: &EntityState,
    action: &str,
) -> Option<u64> {
    caller?.state_timer_armed_seq(tenant, &state.entity_type, &state.entity_id, action)
}

/// Whether `caller` fires a state timer of this record's whose `action` does
/// not reset its clock: a timer that fires once, which its fire's event ends.
pub(super) fn fires_once(
    caller: Option<&AgentContext>,
    tenant: &str,
    state: &EntityState,
    action: &str,
) -> bool {
    caller
        .and_then(|caller| {
            caller.state_timer_fire(tenant, &state.entity_type, &state.entity_id, action)
        })
        .is_some_and(|fire| !fire.repeats)
}

/// Why a timer fire armed at `armed` is dropped on `state`: the record's
/// journal has moved past the arming. None for a dispatch that is not a
/// timer's fire, and for a record at its timer's arming or behind it (an actor
/// behind the journal catches up through the store's sequence check, and the
/// retry asks again after its replay).
pub(super) fn moved_past(armed: Option<u64>, state: &EntityState) -> Option<String> {
    let armed = armed?;
    (state.sequence_nr > armed).then(|| {
        format!(
            "state timer armed at journal sequence {armed} was handled or superseded: \
             the record is at {}",
            state.sequence_nr
        )
    })
}
