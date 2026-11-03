//! How a query-plane catalog row holds a record's state without a second copy
//! of its fields.
//!
//! A catalog row has a `fields` column, the record's query projection, and a
//! `state` column, the rest of the record's state. The `fields` column is a
//! projection of the state's own `fields`: every field except those the spec
//! keeps out of the query plane. So the stored state's `fields` holds only the
//! fields the `fields` column lacks or holds differently, and a reader puts the
//! record's fields back together from both.
//!
//! A row written before this rule carries every field in its state as well.
//! [`catalog_state_from_storage`] reads it the same way: the state's copy wins
//! for any field both hold, so the result is the state that was written.

use serde_json::{Map, Value};

/// Key of the record's fields inside a serialized entity state.
const FIELDS_KEY: &str = "fields";

/// The state to store beside `fields`: `state` with every field that the
/// `fields` column holds with the same value left out of the state's `fields`.
///
/// The state's `fields` key is dropped when nothing is left in it. A state or
/// `fields` value that is not a JSON object is stored as it is.
pub fn catalog_state_for_storage(fields: &Value, state: &Value) -> Value {
    let (Some(column), Some(state)) = (fields.as_object(), state.as_object()) else {
        return state.clone();
    };
    let mut stored = Map::new();
    for (key, value) in state {
        if key == FIELDS_KEY
            && let Value::Object(record_fields) = value
        {
            let rest: Map<String, Value> = record_fields
                .iter()
                .filter(|(name, value)| column.get(name.as_str()) != Some(*value))
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect();
            if !rest.is_empty() {
                stored.insert(key.clone(), Value::Object(rest));
            }
            continue;
        }
        stored.insert(key.clone(), value.clone());
    }
    Value::Object(stored)
}

/// The record's full state from a stored `state` and the row's `fields`
/// column: the column's fields with the stored state's `fields` laid over them.
///
/// The inverse of [`catalog_state_for_storage`]; a stored state that is not a
/// JSON object is returned as it is.
pub fn catalog_state_from_storage(fields: &Value, mut stored: Value) -> Value {
    let Some(state) = stored.as_object_mut() else {
        return stored;
    };
    let record_fields = match (fields, state.remove(FIELDS_KEY)) {
        (Value::Object(column), Some(Value::Object(rest))) => {
            let mut record_fields = column.clone();
            record_fields.extend(rest);
            Value::Object(record_fields)
        }
        (_, Some(stored_fields)) => stored_fields,
        (column, None) => column.clone(),
    };
    state.insert(FIELDS_KEY.to_string(), record_fields);
    stored
}

#[cfg(test)]
#[path = "catalog_state_test.rs"]
mod tests;
