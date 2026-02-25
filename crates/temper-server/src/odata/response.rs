//! OData response-shaping helpers.

use serde_json::Value;

/// Attach OData context/id annotations to an entity JSON body.
pub(super) fn annotate_entity(mut body: Value, context: String, odata_id: Option<String>) -> Value {
    if let Some(obj) = body.as_object_mut() {
        obj.insert("@odata.context".to_string(), serde_json::json!(context));
        if let Some(id) = odata_id {
            obj.insert("@odata.id".to_string(), serde_json::json!(id));
        }
    }
    body
}
