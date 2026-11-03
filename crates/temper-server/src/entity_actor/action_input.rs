//! Resolve stored comparison values before the pure action interpreter runs.

use serde_json::{Map, Value};
use temper_jit::table::TransitionTable;

use super::EntityState;
use super::effects::{FieldSyncMode, ProcessResult, process_action_with_xref_and_field_mode};
use crate::blobs::BlobReadSource;

/// Resolve committed or staged comparison bytes, then run the shared interpreter.
pub(crate) async fn process_action_with_blob_prestate(
    state: &mut EntityState,
    table: &TransitionTable,
    action: &str,
    params: &Value,
    related: &temper_jit::table::RelatedMap,
    mode: FieldSyncMode,
    blob_source: BlobReadSource<'_>,
) -> ProcessResult {
    try_process_action_with_blob_prestate(state, table, action, params, related, mode, blob_source)
        .await
        .unwrap_or_else(ProcessResult::refused)
}

/// Keep an unreadable input distinct from an interpreter refusal. A preview
/// cannot predict a later retry's result when its inputs could not be read.
pub(crate) async fn try_process_action_with_blob_prestate(
    state: &mut EntityState,
    table: &TransitionTable,
    action: &str,
    params: &Value,
    related: &temper_jit::table::RelatedMap,
    mode: FieldSyncMode,
    blob_source: BlobReadSource<'_>,
) -> Result<ProcessResult, String> {
    let mut fields = Map::new();
    if let Some(contract) = table.action_contracts.get(action) {
        for name in contract
            .constraints
            .iter()
            .filter_map(|constraint| constraint.field())
        {
            if !state.counters.contains_key(name)
                && !state.booleans.contains_key(name)
                && let Some(value) = state.fields.get(name)
            {
                fields.insert(name.to_string(), value.clone());
            }
        }
    }
    // Invariants read the same logical prestate as input constraints.
    for invariant in &table.invariants {
        invariant.assert.for_each_name(&mut |name| {
            if let temper_spec::predicate::Name::Var(name) = name
                && !state.counters.contains_key(name)
                && !state.booleans.contains_key(name)
                && !state.lists.contains_key(name)
                && let Some(value) = state.fields.get(name)
                && (value.is_object() || value.is_array())
            {
                fields.insert(name.to_string(), value.clone());
            }
        });
    }
    let mut fields = Value::Object(fields);
    crate::blobs::hydrate_comparison_fields(&blob_source, &mut fields).await?;
    Ok(process_action_with_xref_and_field_mode(
        state,
        table,
        action,
        params,
        related,
        mode,
        Some(&fields),
    ))
}
