//! The part of a reply the cache keeps ([`replay_copy`]), and the bytes it
//! counts against [`IDEMPOTENCY_CACHE_BYTES_BUDGET`](super::IDEMPOTENCY_CACHE_BYTES_BUDGET)
//! ([`estimated_entry_bytes`]).

use std::collections::BTreeMap;

use crate::entity_actor::{EntityEvent, EntityResponse, EntityState};

/// Bytes counted for every entry on top of its contents: map nodes, the
/// order index and the reply's fixed fields.
const ENTRY_OVERHEAD_BYTES: usize = 256;
/// Bytes counted for every JSON value, event or list item on top of its text.
pub(super) const VALUE_OVERHEAD_BYTES: usize = 32;

/// The part of a reply a replay needs. Post-dispatch effects read event
/// metadata and the status clock (state clocks, source status); event parameters and the durable
/// key set dominate a busy entity's state and are not copied. A trigger fired
/// from a replayed reply that reads `events[].params` sees null there (the
/// same values are in the entity's fields).
pub(super) fn replay_copy(response: &EntityResponse) -> EntityResponse {
    let state = &response.state;
    let events = state
        .events
        .iter()
        .map(|event| EntityEvent {
            action: event.action.clone(),
            from_status: event.from_status.clone(),
            to_status: event.to_status.clone(),
            timestamp: event.timestamp,
            params: serde_json::Value::Null,
            idempotency_key: event.idempotency_key.clone(),
            origin: event.origin.clone(),
            kind: temper_runtime::persistence::EventKind::Transition,
            outcome: None,
        })
        .collect();
    EntityResponse {
        success: response.success,
        refusal: response.refusal.clone(),
        state: EntityState {
            entity_type: state.entity_type.clone(),
            entity_id: state.entity_id.clone(),
            status: state.status.clone(),
            item_count: state.item_count,
            counters: state.counters.clone(),
            booleans: state.booleans.clone(),
            lists: state.lists.clone(),
            fields: state.fields.clone(),
            events,
            total_event_count: state.total_event_count,
            events_since_snapshot: state.events_since_snapshot,
            last_snapshot_sequence_nr: state.last_snapshot_sequence_nr,
            sequence_nr: state.sequence_nr,
            processed_idempotency_keys: BTreeMap::new(),
            last_event: state.last_event.clone(),
            status_clock: state.status_clock.clone(),
            spawn_reservations: state.spawn_reservations.clone(),
        },
        error: response.error.clone(),
        custom_effects: response.custom_effects.clone(),
        scheduled_actions: response.scheduled_actions.clone(),
        spawn_requests: response.spawn_requests.clone(),
        spec_governed: response.spec_governed,
        refused: response.refused,
        answered_action: None,
        trigger_failures: response.trigger_failures.clone(),
        store_unavailable: response.store_unavailable,
    }
}

/// Rough heap bytes one cached reply holds: everything [`replay_copy`] keeps,
/// and the keys that index it (held by both the entry map and its order).
pub(super) fn estimated_entry_bytes(
    actor_key: &str,
    idem_key: &str,
    response: &EntityResponse,
) -> usize {
    let state = &response.state;
    let named = |name: &String| VALUE_OVERHEAD_BYTES + name.len();
    let events: usize = state
        .events
        .iter()
        .map(|event| {
            VALUE_OVERHEAD_BYTES
                + event.action.len()
                + event.from_status.len()
                + event.to_status.len()
                + event.idempotency_key.as_ref().map_or(0, String::len)
                + event.origin.by.as_ref().map_or(0, String::len)
                + event.origin.acting_for.as_ref().map_or(0, String::len)
                + event.origin.agent_type.as_ref().map_or(0, String::len)
                + event.origin.role.as_ref().map_or(0, String::len)
                + event.origin.label.as_ref().map_or(0, String::len)
        })
        .sum();
    let spawns: usize = response
        .spawn_requests
        .iter()
        .map(|spawn| {
            VALUE_OVERHEAD_BYTES
                + spawn.entity_type.len()
                + spawn.entity_id.len()
                + spawn.reservation_id.len()
                + spawn
                    .copied_fields
                    .iter()
                    .map(|(name, value)| name.len() + json_bytes(value))
                    .sum::<usize>()
        })
        .sum();
    let lists: usize = state
        .lists
        .iter()
        .map(|(name, items)| named(name) + items.iter().map(named).sum::<usize>())
        .sum();
    let variables: usize = state.counters.keys().map(named).sum::<usize>()
        + state.booleans.keys().map(named).sum::<usize>();
    let effects: usize = response.custom_effects.iter().map(named).sum::<usize>()
        + response
            .scheduled_actions
            .iter()
            .map(|scheduled| named(&scheduled.action) + scheduled.arm_id.len())
            .sum::<usize>();
    ENTRY_OVERHEAD_BYTES
        + 2 * (actor_key.len() + idem_key.len())
        + state.entity_type.len()
        + state.entity_id.len()
        + state.status.len()
        + [
            &state.last_event.last_event_by,
            &state.last_event.last_event_for,
            &state.last_event.last_event_agent_type,
            &state.last_event.last_event_label,
        ]
        .into_iter()
        .map(|value| value.as_ref().map_or(0, String::len))
        .sum::<usize>()
        + response.error.as_ref().map_or(0, String::len)
        + json_bytes(&state.fields)
        + state
            .spawn_reservations
            .iter()
            .map(|(field, owner)| named(field) + owner.len())
            .sum::<usize>()
        + events
        + spawns
        + lists
        + variables
        + effects
}

/// Text bytes plus a per-value overhead, walked without recursion.
fn json_bytes(value: &serde_json::Value) -> usize {
    let mut total = 0;
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        total += VALUE_OVERHEAD_BYTES;
        match value {
            serde_json::Value::String(text) => total += text.len(),
            serde_json::Value::Array(items) => pending.extend(items),
            serde_json::Value::Object(map) => {
                for (name, item) in map {
                    total += name.len();
                    pending.push(item);
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }
    total
}
