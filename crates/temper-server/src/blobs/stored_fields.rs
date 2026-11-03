//! Required stored values for record validation, independent of response budgets.

use std::collections::BTreeSet;

use serde_json::Value;

use super::field_overflow_descriptor;
use super::hydration::blob_bytes_match_key;
use super::read_source::{BlobReadSource, read_blob_ref_bytes};
use crate::blob_store::BlobReadBounded;

/// Resolve every referenced value in the selected fields, verifying its declared
/// length, content address and JSON. Each read is bounded by the stored length;
/// response/WASM byte and read budgets do not limit which records may be checked.
/// `None` means no selected field held a reference. Unreadable data fails closed.
pub(crate) async fn resolve_stored_fields(
    source: &BlobReadSource<'_>,
    fields: &Value,
    names: &BTreeSet<String>,
) -> Result<Option<Value>, String> {
    let Some(map) = fields.as_object() else {
        return Ok(None);
    };
    let mut pointers = Vec::new();
    for name in names {
        if let Some(value) = map.get(name) {
            let escaped = name.replace('~', "~0").replace('/', "~1");
            collect_references(value, format!("/{escaped}"), &mut pointers);
        }
    }
    if pointers.is_empty() {
        return Ok(None);
    }
    let mut resolved = fields.clone();
    for (pointer, key, declared_size) in pointers {
        let size = usize::try_from(declared_size)
            .map_err(|_| "Stored field length does not fit in memory".to_string())?;
        let bytes = match read_blob_ref_bytes(source, &key, size).await? {
            BlobReadBounded::Found(bytes)
                if bytes.len() == size && blob_bytes_match_key(&key, &bytes) =>
            {
                bytes
            }
            _ => return Err("Stored field blob could not be resolved and verified".to_string()),
        };
        let value = serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
        // The paths come from this unchanged copy of the stored record.
        *resolved
            .pointer_mut(&pointer)
            .expect("stored field path exists") = value;
    }
    Ok(Some(resolved))
}

fn collect_references(value: &Value, pointer: String, out: &mut Vec<(String, String, u64)>) {
    if let Some(descriptor) = field_overflow_descriptor(value) {
        out.push((
            pointer,
            descriptor.key.to_string(),
            descriptor.serialized_bytes,
        ));
        return;
    }
    match value {
        Value::Array(values) => {
            for (index, value) in values.iter().enumerate() {
                collect_references(value, format!("{pointer}/{index}"), out);
            }
        }
        Value::Object(map) => {
            for (name, value) in map {
                let escaped = name.replace('~', "~0").replace('/', "~1");
                collect_references(value, format!("{pointer}/{escaped}"), out);
            }
        }
        _ => {}
    }
}
