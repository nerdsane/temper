//! Column encoding for a persisted trajectory row.
//!
//! Backend-agnostic: every `TrajectorySink` implementation encodes the same
//! way, so a row means the same thing whichever store wrote it.

use crate::state::trajectory::{TrajectoryEntry, TrajectorySource};

pub(super) fn trajectory_source_label(source: &TrajectorySource) -> &'static str {
    match source {
        TrajectorySource::Entity => "Entity",
        TrajectorySource::Platform => "Platform",
        TrajectorySource::Authz => "Authz",
    }
}

/// Maximum stored size, in bytes, of a captured trajectory request body.
pub(crate) const TRAJECTORY_REQUEST_BODY_MAX_BYTES: usize = 4096;

/// Serialize a captured request body for storage, bounded to
/// [`TRAJECTORY_REQUEST_BODY_MAX_BYTES`].
///
/// An oversized body becomes a valid JSON envelope carrying a bounded preview
/// rather than a byte-sliced prefix. The column is read back with
/// `serde_json::from_str`, so a sliced prefix parses as nothing and the row
/// loses its body entirely. Successful actions now record their params too, so
/// a large body is an ordinary case rather than a rare one.
pub(super) fn trajectory_request_body_json(entry: &TrajectoryEntry) -> Option<String> {
    let serialized = serde_json::to_string(entry.request_body.as_ref()?).ok()?;
    if serialized.len() <= TRAJECTORY_REQUEST_BODY_MAX_BYTES {
        return Some(serialized);
    }
    Some(truncated_request_body_value(&serialized).to_string())
}

/// Bound a request body at the moment of capture.
///
/// Applies the same cap as the sink, one step earlier. Every dispatch now
/// records its params, and entries wait in the bounded outbox before they are
/// written, so capturing whole bodies would let a backed-up outbox hold
/// megabytes per in-flight entry. Bounding here keeps the ceiling per entry at
/// [`TRAJECTORY_REQUEST_BODY_MAX_BYTES`].
pub(crate) fn bounded_request_body(value: &serde_json::Value) -> serde_json::Value {
    match serde_json::to_string(value) {
        Ok(serialized) if serialized.len() > TRAJECTORY_REQUEST_BODY_MAX_BYTES => {
            truncated_request_body_value(&serialized)
        }
        _ => value.clone(),
    }
}

/// Build the truncation envelope for an oversized request body.
///
/// JSON string escaping can expand the preview, so the preview shrinks until
/// the rendered envelope fits the cap. An empty preview always fits.
fn truncated_request_body_value(serialized: &str) -> serde_json::Value {
    let original_bytes = serialized.len();
    let mut preview_len = TRAJECTORY_REQUEST_BODY_MAX_BYTES;
    loop {
        while preview_len > 0 && !serialized.is_char_boundary(preview_len) {
            preview_len -= 1;
        }
        let envelope = serde_json::json!({
            "_truncated": true,
            "_original_bytes": original_bytes,
            "_preview": &serialized[..preview_len],
        });
        let rendered_len = envelope.to_string().len();
        if rendered_len <= TRAJECTORY_REQUEST_BODY_MAX_BYTES || preview_len == 0 {
            return envelope;
        }
        let overflow = rendered_len - TRAJECTORY_REQUEST_BODY_MAX_BYTES;
        preview_len = preview_len.saturating_sub(overflow.max(1));
    }
}

pub(super) fn trajectory_matched_policy_ids_json(entry: &TrajectoryEntry) -> Option<String> {
    entry
        .matched_policy_ids
        .as_ref()
        .and_then(|ids| serde_json::to_string(ids).ok())
}

#[cfg(test)]
#[path = "trajectory_row_test.rs"]
mod tests;
