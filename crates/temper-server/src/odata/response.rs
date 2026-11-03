//! OData response-shaping helpers.

use serde_json::{Map, Value};

/// Name every kernel event property on a read, without changing saved state.
/// Single records, catalog rows and actor-backed lists share this shape.
pub(super) fn fill_event_metadata(body: &mut Map<String, Value>) {
    for property in temper_spec::automaton::EVENT_METADATA_FIELDS {
        body.entry(property.to_string()).or_insert(Value::Null);
    }
}

/// Attach OData context/id annotations to an entity JSON body, and name each
/// kernel event property, null when the record has none: a record's state
/// keeps only the ones it has, so a reader never has to tell missing from
/// null. List rows use the same defaults (`read_support`).
pub(super) fn annotate_entity(mut body: Value, context: String, odata_id: Option<String>) -> Value {
    if let Some(obj) = body.as_object_mut() {
        if obj.contains_key("entity_id") {
            fill_event_metadata(obj);
        }
        obj.insert("@odata.context".to_string(), serde_json::json!(context));
        if let Some(id) = odata_id {
            obj.insert("@odata.id".to_string(), serde_json::json!(id));
        }
    }
    body
}
