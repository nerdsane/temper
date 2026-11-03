//! What a File (and a FileVersion) says of its bytes comes from bytes this server stored for it. Its `$value` PUT
//! hashes the bytes it keeps and records `content_hash`, `size_bytes` and `has_content` with `File.StreamUpdated`,
//! which the server dispatches itself; a read then fetches the bytes stored under that hash. So no caller writes
//! those fields: a create, PATCH or PUT naming one is refused, a caller's bound `File.StreamUpdated` is refused, and
//! any other bound action names none of them. A File's `$value` is then only ever bytes sent to that File.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::response::odata_error;

/// The fields only a File's stored bytes decide.
const CONTENT_FIELDS: [&str; 3] = ["content_hash", "has_content", "size_bytes"];

fn holds_file_content(entity_type: &str) -> bool {
    matches!(entity_type, "File" | "FileVersion")
}

/// A create, PATCH or PUT body that names a File's content fields: refused.
pub(super) fn refuse_content_fields(
    entity_type: &str,
    body: &serde_json::Value,
) -> Option<Response> {
    if !holds_file_content(entity_type) {
        return None;
    }
    let named: Vec<&str> = CONTENT_FIELDS
        .into_iter()
        .filter(|field| body.get(field).is_some())
        .collect();
    (!named.is_empty()).then(|| {
        odata_error(
            StatusCode::BAD_REQUEST,
            "FileContentFields",
            &format!(
                "{} of a {entity_type} come only from the bytes its $value PUT stores; send the bytes there instead",
                named.join(", ")
            ),
        )
        .into_response()
    })
}

/// A caller's bound action on a File or FileVersion. `File.StreamUpdated` is the server's own record of bytes a
/// `$value` PUT stored, so a caller's is refused; any other action names none of the content fields.
pub(super) fn refuse_caller_content(
    entity_type: &str,
    action: &str,
    body: &serde_json::Value,
) -> Option<Response> {
    if !holds_file_content(entity_type) {
        return None;
    }
    let action = action.rsplit('.').next().unwrap_or(action);
    if (entity_type, action) == ("File", "StreamUpdated") {
        return Some(
            odata_error(
                StatusCode::BAD_REQUEST,
                "FileContentFields",
                "File.StreamUpdated is recorded by the File's $value PUT, which stores the bytes; send the bytes there instead",
            )
            .into_response(),
        );
    }
    refuse_content_fields(entity_type, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_files_content_fields_are_never_a_callers() {
        for entity_type in ["File", "FileVersion"] {
            for body in [
                serde_json::json!({ "Id": "f1", "content_hash": "sha256:00" }),
                serde_json::json!({ "has_content": true }),
                serde_json::json!({ "size_bytes": 42, "mime_type": "image/png" }),
            ] {
                let refused = refuse_content_fields(entity_type, &body).expect("refused");
                assert_eq!(
                    refused.status(),
                    StatusCode::BAD_REQUEST,
                    "{entity_type} {body}"
                );
            }
            assert!(
                refuse_content_fields(
                    entity_type,
                    &serde_json::json!({ "Id": "f1", "mime_type": "image/png" })
                )
                .is_none()
            );
        }
        // Any other type keeps its own fields of those names.
        assert!(
            refuse_content_fields("Picture", &serde_json::json!({ "content_hash": "x" })).is_none()
        );
    }

    #[test]
    fn a_callers_stream_updated_is_refused_whatever_it_names() {
        let refused = refuse_caller_content(
            "File",
            "Temper.StreamUpdated",
            &serde_json::json!({ "content_hash": "sha256:00", "size_bytes": 1 }),
        )
        .expect("refused");
        assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
        // Other bound actions are refused only when they name a content field.
        assert!(
            refuse_caller_content("File", "Lock", &serde_json::json!({ "reason": "x" })).is_none()
        );
        assert!(
            refuse_caller_content(
                "FileVersion",
                "Create",
                &serde_json::json!({ "content_hash": "sha256:00" })
            )
            .is_some()
        );
    }
}
