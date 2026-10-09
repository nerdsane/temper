//! Generic writes cannot create a second, caller-controlled view of effect-owned state.
//!
//! Ownership includes declarations whose values are absent in legacy state. Reserving a
//! name must not materialize today's declaration default during recovery.

use std::collections::BTreeSet;

use serde_json::Value;
use temper_jit::table::TransitionTable;

use super::{EntityState, effects};

fn owned_names(state: &EntityState, table: &TransitionTable) -> BTreeSet<String> {
    let declared = &table.initial_values;
    declared
        .counters
        .keys()
        .chain(declared.booleans.keys())
        .chain(declared.lists.keys())
        .chain(state.counters.keys())
        .chain(state.booleans.keys())
        .chain(state.lists.keys())
        // These are the same aliases used by key_index and OData nearest reads.
        .flat_map(|name| {
            [
                name.clone(),
                temper_spec::to_snake_case(name),
                temper_spec::to_pascal_case(name),
            ]
        })
        .collect()
}

/// Remove ignored inputs before journaling; never record an invented owned value.
pub(crate) fn sanitize(state: &EntityState, table: &TransitionTable, fields: &Value) -> Value {
    let mut fields = effects::sanitize_action_params(fields).into_owned();
    let owned = owned_names(state, table);
    if let Some(fields) = fields.as_object_mut() {
        fields.retain(|name, _| !owned.contains(name));
    }
    fields
}

/// Publish only actual authoritative values, retaining genuine bounded list projections.
/// Historical corrupt mirrors are repaired, not used to manufacture new state/defaults.
pub(crate) fn normalize(state: &mut EntityState, table: &TransitionTable) {
    let mut fields = sanitize(state, table, &state.fields);
    let Some(obj) = fields.as_object_mut() else {
        return;
    };
    for (name, value) in &state.counters {
        obj.insert(name.clone(), serde_json::json!(value));
    }
    for (name, value) in &state.booleans {
        obj.insert(name.clone(), Value::Bool(*value));
    }
    for (name, value) in &state.lists {
        let value = serde_json::json!(value);
        let metadata = table.state_var_metadata.get(name);
        let mut discarded_blobs = Vec::new();
        let blob = effects::project_field_value(
            name,
            &value,
            effects::FieldSyncMode::blob_refs_default(),
            &state.entity_type,
            &state.entity_id,
            metadata,
            &mut discarded_blobs,
        );
        let projection = if state.fields.get(name) == Some(&blob) {
            blob
        } else {
            effects::project_field_value(
                name,
                &value,
                effects::FieldSyncMode::InlineTruncate,
                &state.entity_type,
                &state.entity_id,
                metadata,
                &mut discarded_blobs,
            )
        };
        // No blob I/O here: only a content-verified existing reference is retained.
        // A missing/forged mirror falls back to the established bounded inline form.
        obj.insert(name.clone(), projection);
    }
    effects::canonicalize_entity_fields(&mut fields, &state.entity_id, &state.status);
    state.fields = fields;
}

/// PATCH merges writable fields; PUT replaces them. Both ignore owned inputs.
pub(crate) fn apply(
    state: &mut EntityState,
    table: &TransitionTable,
    fields: &Value,
    replace: bool,
) -> bool {
    if !fields.is_object() {
        return false;
    }
    let updates = sanitize(state, table, fields);
    normalize(state, table);
    if replace {
        // Carry genuine list projections across PUT rather than turning a blob ref
        // into an unbounded array or introducing blob writes into this transaction.
        let lists: Vec<_> = state
            .lists
            .keys()
            .filter_map(|name| {
                state
                    .fields
                    .get(name)
                    .map(|value| (name.clone(), value.clone()))
            })
            .collect();
        state.fields = updates;
        let obj = state.fields.as_object_mut().expect("object checked above");
        obj.extend(lists);
    } else if let (Some(existing), Some(updates)) =
        (state.fields.as_object_mut(), updates.as_object())
    {
        existing.extend(
            updates
                .iter()
                .map(|(name, value)| (name.clone(), value.clone())),
        );
    }
    normalize(state, table);
    true
}
