//! Reference values shared by production dispatch and deterministic simulation.

use serde_json::Value;
use temper_jit::table::action_contract::InitialValues;

/// Read stored scalar/list IDs, falling back to declared defaults only when absent.
pub(crate) fn reference_ids<'a>(
    fields: &'a Value,
    declared: &'a InitialValues,
    name: &str,
) -> Vec<&'a str> {
    if fields.get(name).is_none()
        && let Some(ids) = declared.lists.get(name)
    {
        return ids
            .iter()
            .map(String::as_str)
            .filter(|id| !id.is_empty())
            .collect();
    }
    match declared.field(fields.get(name), name) {
        Some(Value::Array(ids)) => ids
            .iter()
            .filter_map(Value::as_str)
            .filter(|id| !id.is_empty())
            .collect(),
        Some(Value::String(id)) if !id.is_empty() => vec![id.as_str()],
        _ => Vec::new(),
    }
}
