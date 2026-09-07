//! Resolve stored comparison values before the pure action interpreter runs.
use std::collections::BTreeMap;

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
    cross_entity_booleans: &BTreeMap<String, bool>,
    mode: FieldSyncMode,
    blob_source: BlobReadSource<'_>,
) -> ProcessResult {
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
    let mut fields = Value::Object(fields);
    if let Err(error) = crate::blobs::hydrate_comparison_fields(&blob_source, &mut fields).await {
        return ProcessResult::refused(error);
    }
    process_action_with_xref_and_field_mode(
        state,
        table,
        action,
        params,
        cross_entity_booleans,
        mode,
        Some(&fields),
    )
}
