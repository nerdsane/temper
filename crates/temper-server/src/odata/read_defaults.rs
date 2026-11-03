//! Read the current declarations without changing a record's committed state.

use serde_json::Value;
use temper_jit::table::TransitionTable;
use temper_runtime::tenant::TenantId;

use crate::state::ServerState;

/// Defaults used by guards and effects when an older record stores no value.
/// Resolve them once per entity type, including for catalog-only reads.
pub(crate) struct ReadDefaults(Value);

impl ReadDefaults {
    pub(crate) fn for_entity(state: &ServerState, tenant: &TenantId, entity_type: &str) -> Self {
        let table = state
            .registry
            .read()
            .expect("registry lock should not be poisoned")
            .get_table(tenant, entity_type)
            .or_else(|| state.transition_tables.get(entity_type).cloned());
        table.map_or_else(
            || Self(serde_json::json!({})),
            |table| Self::from_table(&table),
        )
    }

    /// The declarations of `table`.
    pub(crate) fn from_table(table: &TransitionTable) -> Self {
        Self(serde_json::json!(table.initial_values))
    }

    /// A record's `fields` as guards and rules read them: each declared
    /// variable it holds no value for reads as its declared initial value.
    /// A stored value, null included, is kept. `fields` is the record's
    /// field map after a move, which mirrors its counters, booleans and
    /// lists.
    pub(crate) fn fields_as_read(&self, fields: &Value) -> Value {
        let mut read = fields.clone();
        fill_missing(&mut read, &self.field_values(&Value::Null));
        read
    }

    pub(crate) fn apply(&self, body: &mut Value) {
        let fields = self.field_values(body);
        let Some(body) = body.as_object_mut() else {
            return;
        };
        for group in ["counters", "booleans", "lists"] {
            if let Some(defaults) = self.0.get(group) {
                fill_missing(
                    body.entry(group).or_insert_with(|| serde_json::json!({})),
                    defaults,
                );
            }
        }
        fill_missing(
            body.entry("fields")
                .or_insert_with(|| serde_json::json!({})),
            &fields,
        );
    }

    /// Effective declared fields, including the typed values moves mirror into fields.
    pub(crate) fn field_values(&self, body: &Value) -> Value {
        let mut fields = self
            .0
            .get("fields")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        for group in ["counters", "booleans", "lists"] {
            if let Some(defaults) = self.0.get(group).and_then(Value::as_object) {
                for (name, initial) in defaults {
                    if !temper_spec::automaton::is_server_derived_field_name(name) {
                        let value = body
                            .get(group)
                            .and_then(|stored| stored.get(name))
                            .unwrap_or(initial);
                        fields.insert(name.clone(), value.clone());
                    }
                }
            }
        }
        Value::Object(fields)
    }

    /// Sparse reads of a typed field also need its stored typed map, because
    /// older rows can have that value without its mirror in `fields`.
    pub(crate) fn catalog_fields(&self, select: &[String]) -> Vec<String> {
        let mut fields = select.to_vec();
        for group in ["counters", "booleans", "lists"] {
            if let Some(declared) = self.0.get(group).and_then(Value::as_object)
                && declared.keys().any(|name| {
                    select
                        .iter()
                        .any(|property| property == name || property == "fields")
                })
                && !fields.iter().any(|field| field == group)
            {
                fields.push(group.to_string());
            }
        }
        fields
    }

    /// Sparse catalog rows have already projected their requested properties.
    /// Fill those properties only, so `$select` never exposes unselected data.
    pub(crate) fn apply_selected(&self, body: &mut Value, select: &[String], fields: &Value) {
        let Some(body) = body.as_object_mut() else {
            return;
        };
        for property in select {
            if property == "fields" {
                fill_missing(
                    body.entry(property)
                        .or_insert_with(|| serde_json::json!({})),
                    fields,
                );
            } else if let Some(defaults) = self.0.get(property) {
                fill_missing(
                    body.entry(property)
                        .or_insert_with(|| serde_json::json!({})),
                    defaults,
                );
            } else if let Some(value) = fields.get(property) {
                body.entry(property).or_insert_with(|| value.clone());
            }
        }
    }
}

fn fill_missing(stored: &mut Value, defaults: &Value) {
    if stored.is_null() {
        *stored = defaults.clone();
        return;
    }
    if let (Some(stored), Some(defaults)) = (stored.as_object_mut(), defaults.as_object()) {
        for (name, value) in defaults {
            stored.entry(name).or_insert_with(|| value.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stored_values_including_null_and_empty_values_win_over_declarations() {
        let defaults = ReadDefaults(json!({
            "counters": {"stock": 6, "new_count": 7},
            "booleans": {"available": true, "new_flag": true},
            "lists": {"tags": ["new"]},
            "fields": {"label": "fresh", "nullable": "default", "temperature": -2}
        }));
        let mut body = json!({
            "counters": {"stock": 0}, "booleans": {"available": false},
            "lists": {"tags": []}, "fields": {"label": "", "nullable": null}
        });
        defaults.apply(&mut body);
        assert_eq!(
            body,
            json!({
                "counters": {"stock": 0, "new_count": 7},
                "booleans": {"available": false, "new_flag": true},
                "lists": {"tags": []},
                "fields": {"label": "", "nullable": null, "temperature": -2,
                    "stock": 0, "new_count": 7, "available": false, "new_flag": true, "tags": []}
            })
        );
        let full = body.clone();
        defaults.apply(&mut body);
        assert_eq!(body, full, "materializing a read is idempotent");
    }

    #[test]
    fn selected_defaults_merge_sparse_maps_without_exposing_other_fields() {
        let defaults = ReadDefaults(json!({
            "counters": {"stock": 6}, "booleans": {"available": true},
            "fields": {"label": "fresh", "private": "unselected"}
        }));
        let mut body = json!({"counters": {"stock": 0}});
        let fields = defaults.field_values(&body);
        defaults.apply_selected(&mut body, &["counters".into(), "label".into()], &fields);
        assert_eq!(body, json!({"counters": {"stock": 0}, "label": "fresh"}));
    }
    #[test]
    fn sparse_typed_fields_use_stored_values_without_exposing_their_maps() {
        let defaults = ReadDefaults(json!({
            "counters": {"stock": 6}, "booleans": {"available": true}, "lists": {}, "fields": {}
        }));
        let select = vec!["stock".into(), "available".into()];
        assert_eq!(
            defaults.catalog_fields(&select),
            ["stock", "available", "counters", "booleans"]
        );
        let fields = defaults
            .field_values(&json!({"counters": {"stock": 0}, "booleans": {"available": false}}));
        let mut body = json!({});
        defaults.apply_selected(&mut body, &select, &fields);
        assert_eq!(body, json!({"stock": 0, "available": false}));
    }
}
