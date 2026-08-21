//! Validation for content-addressed field-overflow descriptors.

use serde_json::Value;

use super::{
    FIELD_OVERFLOW_BLOB_PREFIX, FIELD_OVERFLOW_ENCODING_KEY, FIELD_OVERFLOW_REF_KEY,
    FIELD_OVERFLOW_SIZE_KEY,
};

/// Validated reference to a field-overflow JSON object.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FieldOverflowDescriptor<'a> {
    /// Content-addressed object-store key.
    pub key: &'a str,
    /// Lowercase SHA-256 of the serialized JSON object.
    pub sha256: &'a str,
    /// Serialized JSON byte length stored under `key`.
    pub serialized_bytes: u64,
}

/// Parse and validate a field-overflow descriptor.
pub fn field_overflow_descriptor(value: &Value) -> Option<FieldOverflowDescriptor<'_>> {
    let object = value.as_object()?;
    let key = object.get(FIELD_OVERFLOW_REF_KEY)?.as_str()?;
    let sha256 = field_overflow_sha256(key)?;
    if object.get(FIELD_OVERFLOW_ENCODING_KEY)?.as_str()? != "json" {
        return None;
    }
    let serialized_bytes = object.get(FIELD_OVERFLOW_SIZE_KEY)?.as_u64()?;
    if serialized_bytes == 0 {
        return None;
    }
    Some(FieldOverflowDescriptor {
        key,
        sha256,
        serialized_bytes,
    })
}

/// Extract the lowercase SHA-256 from a canonical field-overflow key.
pub fn field_overflow_sha256(key: &str) -> Option<&str> {
    let digest = key
        .strip_prefix(FIELD_OVERFLOW_BLOB_PREFIX)?
        .strip_suffix(".json")?;
    (digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then_some(digest)
}

/// Return whether `key` is a canonical field-overflow SHA-256 object key.
pub fn is_valid_field_overflow_key(key: &str) -> bool {
    field_overflow_sha256(key).is_some()
}
