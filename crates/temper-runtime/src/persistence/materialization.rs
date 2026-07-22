//! Bounded state-materialization control records.

use serde::Serialize;

use super::PersistenceEnvelope;

/// Reserved first-journal event that transfers a snapshot-only entity into a
/// self-contained journal generation.
///
/// Stores use this control record, together with an exact snapshot-source fence,
/// to retire the migration snapshot in the same atomic append and to reject a
/// delayed ahead-of-journal snapshot from recreating that retired generation.
pub const STATE_MATERIALIZATION_EVENT_TYPE: &str = "Temper.Internal.StateMaterialization.v1";

/// Payload schema that distinguishes the runtime control record from a legal
/// domain action with the same event-type string.
pub const STATE_MATERIALIZATION_SCHEMA: &str = "temper.state-materialization.v1";

/// Maximum durable idempotency entries accepted in a state-materialization
/// control record.
pub const STATE_MATERIALIZATION_IDEMPOTENCY_KEY_BUDGET: usize = 1_000;

/// Maximum serialized bytes accepted for a state-materialization control payload.
///
/// The control record is created only when a legacy snapshot-only generation
/// first enters the journal. Bounding the one-time baseline prevents an
/// arbitrarily large snapshot from being cloned into an unbounded event.
pub const STATE_MATERIALIZATION_PAYLOAD_BYTE_BUDGET: usize = 16 * 1024 * 1024;

struct JsonByteBudget {
    remaining: usize,
    exhausted: bool,
}

impl std::io::Write for JsonByteBudget {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            self.exhausted = true;
            return Err(std::io::Error::other("JSON byte budget exhausted"));
        }
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Check a value's serialized JSON size without allocating its serialized form.
pub fn serialized_json_fits_byte_budget<T: Serialize>(
    value: &T,
    byte_budget: usize,
) -> Result<bool, String> {
    let mut writer = JsonByteBudget {
        remaining: byte_budget,
        exhausted: false,
    };
    match serde_json::to_writer(&mut writer, value) {
        Ok(()) => Ok(true),
        Err(_) if writer.exhausted => Ok(false),
        Err(error) => Err(error.to_string()),
    }
}

fn object_values_match(
    value: Option<&serde_json::Value>,
    predicate: impl Fn(&serde_json::Value) -> bool,
) -> bool {
    value
        .and_then(serde_json::Value::as_object)
        .is_some_and(|entries| entries.values().all(predicate))
}

/// Return whether an event type and payload form the internal snapshot-to-
/// journal materialization control record for the specified stream identity.
///
/// Event type alone is deliberately insufficient: specs may legally declare an
/// action with the same name. Stores use this exact discriminator before
/// retiring a migration snapshot or suppressing a delayed snapshot writer.
pub fn is_state_materialization_payload_for(
    event_type: &str,
    payload: &serde_json::Value,
    entity_type: &str,
    entity_id: &str,
) -> bool {
    if event_type != STATE_MATERIALIZATION_EVENT_TYPE
        || payload.get("schema").and_then(serde_json::Value::as_str)
            != Some(STATE_MATERIALIZATION_SCHEMA)
        || !matches!(
            serialized_json_fits_byte_budget(payload, STATE_MATERIALIZATION_PAYLOAD_BYTE_BUDGET),
            Ok(true)
        )
    {
        return false;
    }
    let Some(state) = payload.get("state").and_then(serde_json::Value::as_object) else {
        return false;
    };
    let Some(status) = state.get("status").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let fields_match = state
        .get("fields")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|fields| {
            fields.get("Id").and_then(serde_json::Value::as_str) == Some(entity_id)
                && fields.get("Status").and_then(serde_json::Value::as_str) == Some(status)
        });
    let processed_keys_match = state
        .get("processed_idempotency_keys")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|entries| {
            entries.len() <= STATE_MATERIALIZATION_IDEMPOTENCY_KEY_BUDGET
                && entries.values().all(|value| value.as_u64().is_some())
        });

    state.get("entity_type").and_then(serde_json::Value::as_str) == Some(entity_type)
        && state.get("entity_id").and_then(serde_json::Value::as_str) == Some(entity_id)
        && state
            .get("item_count")
            .and_then(serde_json::Value::as_u64)
            .is_some()
        && object_values_match(state.get("counters"), |value| value.as_u64().is_some())
        && object_values_match(state.get("booleans"), |value| value.as_bool().is_some())
        && object_values_match(state.get("lists"), |value| {
            value
                .as_array()
                .is_some_and(|items| items.iter().all(|item| item.as_str().is_some()))
        })
        && fields_match
        && state
            .get("events")
            .and_then(serde_json::Value::as_array)
            .is_some_and(Vec::is_empty)
        && state
            .get("total_event_count")
            .and_then(serde_json::Value::as_u64)
            .is_some()
        && state
            .get("events_since_snapshot")
            .and_then(serde_json::Value::as_u64)
            == Some(0)
        && state
            .get("last_snapshot_sequence_nr")
            .and_then(serde_json::Value::as_u64)
            == Some(0)
        && state.get("sequence_nr").and_then(serde_json::Value::as_u64) == Some(0)
        && processed_keys_match
}

/// Return whether an envelope is the valid materialization control record for
/// the specified stream identity.
pub fn is_state_materialization_event_for(
    envelope: &PersistenceEnvelope,
    entity_type: &str,
    entity_id: &str,
) -> bool {
    is_state_materialization_payload_for(
        &envelope.event_type,
        &envelope.payload,
        entity_type,
        entity_id,
    )
}
