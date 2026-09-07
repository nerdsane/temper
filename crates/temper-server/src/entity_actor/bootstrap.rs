//! Initial state is a creation fact, never a recovery-time declaration default.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use temper_runtime::actor::ActorError;

use super::{EntityEvent, EntityState};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InitialValues {
    fields: Value,
    counters: BTreeMap<String, usize>,
    booleans: BTreeMap<String, bool>,
    lists: BTreeMap<String, Vec<String>>,
}

pub(crate) fn event_payload(
    event: &EntityEvent,
    initial: &EntityState,
) -> Result<Value, serde_json::Error> {
    let mut payload = serde_json::to_value(event)?;
    if event.action == "Created" && event.from_status.is_empty() {
        payload["initial_values"] = serde_json::to_value(InitialValues {
            fields: super::effects::sanitize_action_params(&initial.fields).into_owned(),
            counters: initial.counters.clone(),
            booleans: initial.booleans.clone(),
            lists: initial.lists.clone(),
        })?;
    }
    Ok(payload)
}

/// Clear speculative constructor defaults before reading existing full history.
/// Legacy events have only their committed params; missing values stay missing.
pub(super) fn clear_for_replay(state: &mut EntityState, initial_status: &str) {
    state.status = initial_status.to_string();
    state.events.clear();
    state.total_event_count = 0;
    state.events_since_snapshot = 0;
    state.last_snapshot_sequence_nr = 0;
    state.sequence_nr = 0;
    state.processed_idempotency_keys.clear();
    state.item_count = 0;
    state.counters.clear();
    state.booleans.clear();
    state.lists.clear();
    state.fields = serde_json::json!({});
    super::effects::canonicalize_entity_fields(&mut state.fields, &state.entity_id, &state.status);
}

pub(super) fn restore(state: &mut EntityState, payload: &Value) -> Result<(), ActorError> {
    let Some(values) = payload.get("initial_values") else {
        return Ok(());
    };
    let values: InitialValues = serde_json::from_value(values.clone()).map_err(|error| {
        ActorError::custom(format!("invalid committed initial values: {error}"))
    })?;
    if !values.fields.is_object() {
        return Err(ActorError::custom(
            "committed initial fields must be an object",
        ));
    }
    state.fields = values.fields;
    state.counters = values.counters;
    state.booleans = values.booleans;
    state.lists = values.lists;
    super::effects::canonicalize_entity_fields(&mut state.fields, &state.entity_id, &state.status);
    Ok(())
}
