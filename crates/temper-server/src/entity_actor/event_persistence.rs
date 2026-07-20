//! Durable entity-event payloads and state-timeout clock facts.
//!
//! Every production writer of an [`EntityEvent`] uses this module so the
//! event and its table-at-commit timeout interpretation stay one atomic
//! persistence fact.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use temper_jit::table::TransitionTable;

use super::{EntityEvent, EntityState};

/// Reserved journal-payload key for the table-at-commit timeout clock.
pub(crate) const STATE_TIMEOUT_CLOCK_PAYLOAD_KEY: &str = "__temper_state_timeout_clock";

/// Snapshot marker proving its clock pair is an authoritative durable fact.
pub(crate) const STATE_TIMEOUT_CLOCK_SNAPSHOT_AUTHORITY_KEY: &str =
    "__temper_state_timeout_clock_authoritative";

/// Canonical journal event type for a terminal entity transition.
pub(crate) const ENTITY_TOMBSTONE_EVENT_TYPE: &str = "Deleted";

/// Choose the stable journal event type for a committed entity event.
///
/// Domain specs may name their transition `Delete`, `Remove`, or something
/// else. Durable enumeration and recovery use the resulting terminal state,
/// so every new transition into `Deleted` is encoded with one canonical type.
pub(crate) fn entity_event_type(event: &EntityEvent) -> &str {
    if event.to_status == ENTITY_TOMBSTONE_EVENT_TYPE {
        ENTITY_TOMBSTONE_EVENT_TYPE
    } else {
        &event.action
    }
}

/// Whether a durable envelope represents a terminal entity tombstone.
///
/// Payload inspection preserves compatibility with events written before
/// terminal transitions were normalized to [`ENTITY_TOMBSTONE_EVENT_TYPE`].
pub(crate) fn is_entity_tombstone(event_type: &str, payload: &Value) -> bool {
    match payload.get("to_status").and_then(Value::as_str) {
        Some(status) => status == ENTITY_TOMBSTONE_EVENT_TYPE,
        None => event_type == ENTITY_TOMBSTONE_EVENT_TYPE,
    }
}

/// Timeout-clock outcome co-committed with a domain event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PersistedStateTimeoutClock {
    /// The committing table declared no timeout for the event's target state.
    Inactive,
    /// The committing table declared a timeout with this durable clock identity.
    Active {
        reset_at: chrono::DateTime<chrono::Utc>,
        reset_version: u64,
    },
}

#[derive(Serialize)]
struct PersistedEntityEvent<'a> {
    #[serde(flatten)]
    event: &'a EntityEvent,
    #[serde(rename = "__temper_state_timeout_clock")]
    state_timeout_clock: PersistedStateTimeoutClock,
}

fn retained_state_timeout_reset_at(
    state: &EntityState,
    timeout: &temper_spec::automaton::StateTimeout,
) -> Option<chrono::DateTime<chrono::Utc>> {
    let entry_idx = state
        .events
        .iter()
        .rposition(|event| event.to_status == timeout.state && event.from_status != timeout.state);
    let mut reset_at = entry_idx.map(|idx| state.events[idx].timestamp);
    let scan_from = entry_idx.map_or(0, |idx| idx + 1);
    for event in state.events.iter().skip(scan_from) {
        if event.to_status == timeout.state
            && timeout
                .reset_on
                .iter()
                .any(|action| action == &event.action)
        {
            reset_at = Some(reset_at.map_or(event.timestamp, |at| at.max(event.timestamp)));
        }
    }
    reset_at
}

/// Compute the timeout-clock outcome using exactly the table that commits an event.
pub(crate) fn state_timeout_clock_after_event(
    table: &TransitionTable,
    state: &EntityState,
    event: &EntityEvent,
    event_version: u64,
) -> PersistedStateTimeoutClock {
    let Some(timeout) = table
        .state_timeouts
        .iter()
        .find(|timeout| timeout.state == event.to_status)
    else {
        return PersistedStateTimeoutClock::Inactive;
    };

    let entered_state = event.from_status != event.to_status;
    let reset_clock = timeout
        .reset_on
        .iter()
        .any(|action| action == &event.action);
    let (reset_at, reset_version) = if entered_state || reset_clock {
        (event.timestamp, event_version)
    } else if let Some(reset_at) = state.state_timeout_clock_reset_at {
        (
            reset_at,
            state
                .state_timeout_clock_reset_version
                .unwrap_or(event_version),
        )
    } else {
        // A table may gain a timeout after actor startup captured its prior
        // definition. Preserve a retained entry/reset instead of moving the
        // deadline to an unrelated event. If the bounded tail has no such
        // fact, retain the conservative one-budget fallback at this event.
        (
            retained_state_timeout_reset_at(state, timeout).unwrap_or(event.timestamp),
            event_version,
        )
    };

    PersistedStateTimeoutClock::Active {
        reset_at,
        reset_version,
    }
}

/// Encode one current entity event and its timeout-clock outcome together.
pub(crate) fn encode_entity_event_payload(
    table: &TransitionTable,
    state: &EntityState,
    event: &EntityEvent,
    event_version: u64,
) -> Result<(Value, PersistedStateTimeoutClock), serde_json::Error> {
    let clock = state_timeout_clock_after_event(table, state, event, event_version);
    let payload = serde_json::to_value(PersistedEntityEvent {
        event,
        state_timeout_clock: clock,
    })?;
    Ok((payload, clock))
}

/// Decode the reserved clock fact without conflating absence with corruption.
///
/// `Ok(None)` is reserved for legacy payloads where the key is absent. Once
/// the key is present, `null`, shape errors, and type errors are fatal.
pub(crate) fn decode_entity_event_clock(
    persistence_id: &str,
    event_sequence: u64,
    payload: &Value,
) -> Result<Option<PersistedStateTimeoutClock>, String> {
    let Some(raw_clock) = payload
        .as_object()
        .and_then(|object| object.get(STATE_TIMEOUT_CLOCK_PAYLOAD_KEY))
    else {
        return Ok(None);
    };

    let clock = serde_json::from_value::<PersistedStateTimeoutClock>(raw_clock.clone()).map_err(
        |error| {
            format!(
                "invalid persisted state-timeout clock metadata at sequence {event_sequence} for \
                 {persistence_id}: {error}"
            )
        },
    )?;
    if let PersistedStateTimeoutClock::Active { reset_version, .. } = clock
        && (reset_version == 0 || reset_version > event_sequence)
    {
        return Err(format!(
            "invalid persisted state-timeout clock at sequence {event_sequence} for \
             {persistence_id}: reset_version={reset_version}"
        ));
    }
    Ok(Some(clock))
}

/// Apply a calculated or decoded timeout clock to actor state.
pub(crate) fn apply_state_timeout_clock(
    state: &mut EntityState,
    clock: PersistedStateTimeoutClock,
) {
    match clock {
        PersistedStateTimeoutClock::Inactive => {
            state.state_timeout_clock_reset_at = None;
            state.state_timeout_clock_reset_version = None;
        }
        PersistedStateTimeoutClock::Active {
            reset_at,
            reset_version,
        } => {
            state.state_timeout_clock_reset_at = Some(reset_at);
            state.state_timeout_clock_reset_version = Some(reset_version);
        }
    }
}

fn state_clock_pair(
    persistence_id: &str,
    event_sequence: u64,
    state: &EntityState,
) -> Result<Option<(chrono::DateTime<chrono::Utc>, u64)>, String> {
    match (
        state.state_timeout_clock_reset_at,
        state.state_timeout_clock_reset_version,
    ) {
        (None, None) => Ok(None),
        (Some(reset_at), Some(reset_version)) => Ok(Some((reset_at, reset_version))),
        (reset_at, reset_version) => Err(format!(
            "invalid authoritative state-timeout clock before sequence {event_sequence} for \
             {persistence_id}: reset_at={reset_at:?}, reset_version={reset_version:?}"
        )),
    }
}

/// Apply one present, current-format clock during replay.
///
/// The first present payload after legacy history is an authoritative
/// checkpoint and may retain an older reset identity. Once authority exists,
/// a clock may only stay identical, clear, or establish a new identity at the
/// current envelope sequence. An inactive historical outcome is migrated
/// deterministically when the current table has since added a timeout.
pub(crate) fn apply_replayed_state_timeout_clock(
    persistence_id: &str,
    table: &TransitionTable,
    state: &mut EntityState,
    event: &EntityEvent,
    event_sequence: u64,
    clock: PersistedStateTimeoutClock,
    clock_was_authoritative: bool,
) -> Result<bool, String> {
    if clock == PersistedStateTimeoutClock::Inactive
        && table
            .state_timeouts
            .iter()
            .any(|timeout| timeout.state == event.to_status)
    {
        let migrated = state_timeout_clock_after_event(table, state, event, event_sequence);
        debug_assert!(matches!(
            migrated,
            PersistedStateTimeoutClock::Active { .. }
        ));
        apply_state_timeout_clock(state, migrated);
        return Ok(false);
    }

    if clock_was_authoritative
        && let PersistedStateTimeoutClock::Active {
            reset_at,
            reset_version,
        } = clock
    {
        let previous = state_clock_pair(persistence_id, event_sequence, state)?;
        let retained_exactly = previous == Some((reset_at, reset_version));
        if !retained_exactly && reset_version != event_sequence {
            return Err(format!(
                "non-monotonic persisted state-timeout clock at sequence {event_sequence} for \
                 {persistence_id}: previous={previous:?}, \
                 next=({reset_at:?}, {reset_version})"
            ));
        }
    }

    apply_state_timeout_clock(state, clock);
    Ok(true)
}

/// Apply a legacy event under the current table and mark the result derived.
pub(crate) fn apply_legacy_state_timeout_clock(
    table: &TransitionTable,
    state: &mut EntityState,
    event: &EntityEvent,
    event_version: u64,
) {
    let clock = state_timeout_clock_after_event(table, state, event, event_version);
    apply_state_timeout_clock(state, clock);
}
