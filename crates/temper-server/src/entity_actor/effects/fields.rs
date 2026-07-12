//! Entity-field projection and overflow handling.

use sha2::{Digest, Sha256};

use crate::blobs::{FIELD_OVERFLOW_BLOB_PREFIX, OverflowBlobWrite, blob_ref_value};

use super::super::types::EntityState;
use super::core::FieldSyncMode;

/// Default inline ceiling for a single field value projected into entity state.
///
/// Values above this size are either truncated (`InlineTruncate`) or moved to
/// the content-addressed blob store (`BlobRefs`) per ADR-0040. The ceiling is
/// sized to fit comfortably inside `CTX_BUF_LEN` (512 KB) with headroom for the
/// rest of `entity_state` (counters, booleans, lists, other fields) while
/// covering p99 of observed oversize-field traffic.
///
/// See ADR-0045.
pub const DEFAULT_FIELD_INLINE_MAX: usize = 131_072; // 128 KB

/// Sync all state variables into the `fields` JSON object.
///
/// Projects status, counters, booleans, lists, and action params into the
/// entity's fields for OData queries. Values whose serialized size exceeds
/// the effective per-field inline ceiling are either truncated or projected
/// through blob refs, depending on `mode`. When `state_var_metadata` is
/// `Some`, per-field `overflow_inline_max_bytes` and `overflow_ttl_seconds`
/// overrides are consulted (ADR-0045, ADR-0047).
pub fn sync_fields(
    state: &mut EntityState,
    params: &serde_json::Value,
    mode: FieldSyncMode,
) -> Vec<OverflowBlobWrite> {
    sync_fields_with_metadata(state, params, mode, None)
}

/// Metadata-aware variant of [`sync_fields`]. Threads per-field overflow
/// declarations from the IOA spec's `[[state]]` blocks into the projection.
pub fn sync_fields_with_metadata(
    state: &mut EntityState,
    params: &serde_json::Value,
    mode: FieldSyncMode,
    state_var_metadata: Option<
        &std::collections::BTreeMap<String, temper_jit::table::StateVarMetadata>,
    >,
) -> Vec<OverflowBlobWrite> {
    let mut overflow_blobs = Vec::new();
    let entity_type = state.entity_type.clone();
    let entity_id = state.entity_id.clone();
    if let Some(obj) = state.fields.as_object_mut() {
        obj.insert(
            "Status".to_string(),
            serde_json::Value::String(state.status.clone()),
        );
        prune_transient_action_fields(&entity_type, obj);
        // Project action params into fields
        if let Some(p) = params.as_object() {
            for (k, v) in p {
                if is_transient_action_field(&entity_type, k) {
                    continue;
                }
                let field_meta = state_var_metadata.and_then(|m| m.get(k.as_str()));
                obj.insert(
                    k.clone(),
                    project_field_value(
                        k,
                        v,
                        mode,
                        &entity_type,
                        &entity_id,
                        field_meta,
                        &mut overflow_blobs,
                    ),
                );
            }
        }
        // Sync counters into fields
        for (k, v) in &state.counters {
            obj.insert(k.clone(), serde_json::Value::Number((*v as u64).into()));
        }
        // Sync booleans into fields
        for (k, v) in &state.booleans {
            obj.insert(k.clone(), serde_json::Value::Bool(*v));
        }
        // Sync lists into fields
        for (k, v) in &state.lists {
            let arr: Vec<serde_json::Value> = v
                .iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect();
            let field_meta = state_var_metadata.and_then(|m| m.get(k.as_str()));
            obj.insert(
                k.clone(),
                project_field_value(
                    k,
                    &serde_json::Value::Array(arr),
                    mode,
                    &entity_type,
                    &entity_id,
                    field_meta,
                    &mut overflow_blobs,
                ),
            );
        }
    }
    overflow_blobs
}

fn prune_transient_action_fields(
    entity_type: &str,
    fields: &mut serde_json::Map<String, serde_json::Value>,
) {
    if entity_type != "Repository" {
        return;
    }
    fields.remove("PackBytes");
    fields.remove("RefUpdates");
    fields.remove("ClientRequestId");
}

fn is_transient_action_field(entity_type: &str, field_name: &str) -> bool {
    entity_type == "Repository"
        && matches!(field_name, "PackBytes" | "RefUpdates" | "ClientRequestId")
}

pub(crate) fn prune_transient_action_fields_from_state(state: &mut EntityState) {
    if let Some(obj) = state.fields.as_object_mut() {
        prune_transient_action_fields(&state.entity_type, obj);
    }
}

fn project_field_value(
    field_name: &str,
    value: &serde_json::Value,
    mode: FieldSyncMode,
    entity_type: &str,
    entity_id: &str,
    field_meta: Option<&temper_jit::table::StateVarMetadata>,
    overflow_blobs: &mut Vec<OverflowBlobWrite>,
) -> serde_json::Value {
    let serialized = serde_json::to_vec(value).unwrap_or_else(|_| value.to_string().into_bytes());
    let serialized_len = serialized.len();
    // Per-field override wins over the mode default; mode default wins over
    // crate default (baked into FieldSyncMode::inline_max).
    let inline_max = field_meta
        .and_then(|m| m.overflow_inline_max_bytes)
        .unwrap_or_else(|| mode.inline_max());
    if serialized_len <= inline_max {
        return value.clone();
    }

    match mode {
        FieldSyncMode::InlineTruncate => {
            tracing::warn!(
                entity_type,
                entity_id,
                field = field_name,
                size_bytes = serialized_len,
                inline_max,
                "field truncated under InlineTruncate store — value replaced with placeholder; \
                 consider migrating this tenant to a Turso-backed store to preserve large values"
            );
            serde_json::Value::String(format!(
                "[truncated: {} bytes exceeds {} limit]",
                serialized_len, inline_max
            ))
        }
        FieldSyncMode::BlobRefs { .. } => {
            let digest = Sha256::digest(&serialized);
            let blob_key = format!("{FIELD_OVERFLOW_BLOB_PREFIX}{digest:x}.json");
            let ttl_seconds = field_meta.and_then(|m| m.overflow_ttl_seconds);
            if !overflow_blobs.iter().any(|blob| blob.key == blob_key) {
                overflow_blobs.push(OverflowBlobWrite {
                    key: blob_key.clone(),
                    body: serialized,
                    ttl_seconds,
                });
            }
            blob_ref_value(&blob_key, serialized_len)
        }
    }
}
