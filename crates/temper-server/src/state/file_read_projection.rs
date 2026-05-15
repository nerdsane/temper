use crate::storage::QueryProjectionFieldsRow;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FileProjectionMeta {
    pub(super) content_hash: String,
    pub(super) mime_type: String,
    pub(super) has_content: bool,
}

pub(super) fn file_projection_from_row(row: QueryProjectionFieldsRow) -> FileProjectionMeta {
    FileProjectionMeta {
        content_hash: row
            .fields
            .get("content_hash")
            .cloned()
            .flatten()
            .unwrap_or_default(),
        mime_type: row
            .fields
            .get("mime_type")
            .cloned()
            .flatten()
            .unwrap_or_default(),
        has_content: row
            .fields
            .get("has_content")
            .and_then(|value| value.as_deref())
            .is_some_and(|value| value.eq_ignore_ascii_case("true")),
    }
}

pub(super) fn file_projection_from_state(fields: &serde_json::Value) -> FileProjectionMeta {
    FileProjectionMeta {
        content_hash: fields
            .get("content_hash")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        mime_type: fields
            .get("mime_type")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        has_content: fields
            .get("has_content")
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
    }
}

pub(super) fn file_version_projection_from_row(
    row: QueryProjectionFieldsRow,
) -> FileProjectionMeta {
    let content_hash = row
        .fields
        .get("content_hash")
        .cloned()
        .flatten()
        .unwrap_or_default();
    FileProjectionMeta {
        has_content: !content_hash.is_empty(),
        content_hash,
        mime_type: row
            .fields
            .get("mime_type")
            .cloned()
            .flatten()
            .unwrap_or_default(),
    }
}

pub(super) fn file_version_projection_from_state(fields: &serde_json::Value) -> FileProjectionMeta {
    let content_hash = fields
        .get("content_hash")
        .and_then(|value| value.as_str())
        .unwrap_or_default()
        .to_string();
    FileProjectionMeta {
        has_content: !content_hash.is_empty(),
        content_hash,
        mime_type: fields
            .get("mime_type")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
    }
}
