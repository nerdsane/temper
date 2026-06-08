use std::collections::BTreeMap;

use axum::http::HeaderMap;

const OBSERVATION_METADATA_HEADER: &str = "x-temper-observe-metadata";
const OBSERVATION_METADATA_HEADER_PREFIX: &str = "x-temper-observe-meta-";
const MAX_OBSERVATION_METADATA_KEYS: usize = 32;
const MAX_OBSERVATION_METADATA_KEY_BYTES: usize = 96;
const MAX_OBSERVATION_METADATA_VALUE_BYTES: usize = 1024;

pub(crate) fn extract(headers: &HeaderMap) -> BTreeMap<String, String> {
    let mut metadata = BTreeMap::new();

    if let Some(raw) = header_string(headers, OBSERVATION_METADATA_HEADER)
        && raw.len() <= 8192
        && let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(&raw)
    {
        for (key, value) in map {
            if let Some(value) = metadata_value_string(&value) {
                insert_metadata(&mut metadata, &key, value);
            }
        }
    }

    for (name, value) in headers {
        let name = name.as_str();
        let Some(key) = name.strip_prefix(OBSERVATION_METADATA_HEADER_PREFIX) else {
            continue;
        };
        let Some(value) = value
            .to_str()
            .ok()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
        else {
            continue;
        };
        insert_metadata(&mut metadata, key, value);
    }

    metadata
}

fn header_string(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

fn metadata_value_string(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::String(s) => Some(s.trim().to_string()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        serde_json::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
    .filter(|s| !s.is_empty())
}

fn valid_metadata_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_OBSERVATION_METADATA_KEY_BYTES
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn truncate_metadata_value(value: &str) -> String {
    if value.len() <= MAX_OBSERVATION_METADATA_VALUE_BYTES {
        return value.to_string();
    }
    let mut end = MAX_OBSERVATION_METADATA_VALUE_BYTES;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_string()
}

fn insert_metadata(metadata: &mut BTreeMap<String, String>, key: &str, value: String) {
    if metadata.len() >= MAX_OBSERVATION_METADATA_KEYS || !valid_metadata_key(key) {
        return;
    }
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    metadata.insert(key.to_string(), truncate_metadata_value(value));
}
