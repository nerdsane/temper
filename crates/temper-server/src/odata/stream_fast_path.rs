use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_runtime::tenant::TenantId;

use crate::response::{ODataStreamResponse, odata_error};
use crate::state::{IndexedFileStreamRead, ServerState};

pub(crate) async fn try_file_stream_fast_path(
    state: &ServerState,
    tenant: &TenantId,
    set_name: &str,
    entity_type: &str,
    key: &str,
) -> Option<Response> {
    match entity_type {
        "File" => handle_indexed_file_stream(state, tenant, set_name, entity_type, key).await,
        "FileVersion" => {
            handle_indexed_file_version_stream(state, tenant, set_name, entity_type, key).await
        }
        _ => None,
    }
}

async fn handle_indexed_file_stream(
    state: &ServerState,
    tenant: &TenantId,
    set_name: &str,
    entity_type: &str,
    key: &str,
) -> Option<Response> {
    match state.read_file_stream_indexed(tenant, key).await {
        Ok(read) => indexed_read_response_or_fallback(tenant, set_name, entity_type, key, read),
        Err(error) => Some(index_error_response(tenant, entity_type, key, error)),
    }
}

async fn handle_indexed_file_version_stream(
    state: &ServerState,
    tenant: &TenantId,
    set_name: &str,
    entity_type: &str,
    key: &str,
) -> Option<Response> {
    match state.read_file_version_stream_indexed(tenant, key).await {
        Ok(read) => indexed_read_response_or_fallback(tenant, set_name, entity_type, key, read),
        Err(error) => Some(index_error_response(tenant, entity_type, key, error)),
    }
}

fn indexed_read_response_or_fallback(
    tenant: &TenantId,
    set_name: &str,
    entity_type: &str,
    key: &str,
    read: IndexedFileStreamRead,
) -> Option<Response> {
    match read {
        IndexedFileStreamRead::Content {
            content_hash,
            mime_type,
            bytes,
        } => Some(
            ODataStreamResponse {
                status: StatusCode::OK,
                body: bytes,
                content_type: stream_content_type(&mime_type),
                etag: Some(content_hash),
            }
            .into_response(),
        ),
        IndexedFileStreamRead::NoContent { .. } => Some(
            odata_error(
                StatusCode::NOT_FOUND,
                "NoContent",
                &format!("{set_name}('{key}') has no content yet"),
            )
            .into_response(),
        ),
        IndexedFileStreamRead::MissingIndex => {
            tracing::warn!(
                tenant = %tenant,
                entity_type = %entity_type,
                entity_id = %key,
                "file stream projection missing; falling back to actor/WASM materialization"
            );
            None
        }
        IndexedFileStreamRead::StaleIndex { content_hash, .. } => {
            tracing::warn!(
                tenant = %tenant,
                entity_type = %entity_type,
                entity_id = %key,
                content_hash = %content_hash,
                "file stream projection blob missing; falling back to actor/WASM materialization"
            );
            None
        }
    }
}

fn index_error_response(
    tenant: &TenantId,
    entity_type: &str,
    key: &str,
    error: String,
) -> Response {
    tracing::error!(
        tenant = %tenant,
        entity_type = %entity_type,
        entity_id = %key,
        %error,
        "file stream projection read failed"
    );
    odata_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "FileReadIndexUnavailable",
        &error,
    )
    .into_response()
}

fn stream_content_type(mime_type: &str) -> String {
    if mime_type.trim().is_empty() {
        "application/octet-stream".to_string()
    } else {
        mime_type.to_string()
    }
}

/// How long a link to a File's bytes works by default (`redirect=1`). A reader is checked again for every new link.
const FILE_LINK_SECS: u64 = 5 * 60;

/// How long the link a `redirect` value asks for works: `1` (or `true`) five minutes, a number of seconds from 60 up
/// to an hour (a film plays through one link, whose ranges a player asks for as it goes). `None` asks for no link.
pub(crate) fn file_link_secs(value: &str) -> Option<u64> {
    match value {
        "1" | "true" => Some(FILE_LINK_SECS),
        secs => secs
            .parse::<u64>()
            .ok()
            .filter(|s| *s >= 60)
            .map(|s| s.min(crate::blob_store::MAX_PRESIGN_SECS)),
    }
}

/// A `302` to a short-lived object-store link to an authorized File's bytes, labelled with the type it was stored
/// as. Only pictures, films and sounds open in place; anything else (an SVG included, which can run script) comes
/// as a download. `None` when the File has no bytes yet or the tenant's store cannot sign one (a local store): the
/// caller then serves the bytes itself.
pub(crate) fn file_link_response(
    state: &ServerState,
    tenant: &TenantId,
    entity_state: &serde_json::Value,
    secs: u64,
) -> Option<Response> {
    let fields = entity_state.get("fields")?;
    let content_hash = fields
        .get("content_hash")?
        .as_str()
        .filter(|hash| !hash.is_empty())?;
    let mime_type = fields
        .get("mime_type")
        .and_then(|value| value.as_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("application/octet-stream");
    let store = state.blob_store_for_tenant(tenant).ok()?;
    let kind = mime_type.to_ascii_lowercase();
    let opens_in_place = ["image/", "video/", "audio/"]
        .iter()
        .any(|prefix| kind.starts_with(prefix))
        && !kind.starts_with("image/svg");
    let url = store.presigned_get_url(
        &crate::state::native_file_blob_key(content_hash),
        secs,
        &crate::blob_store::PresignedResponse {
            content_type: Some(mime_type),
            content_disposition: (!opens_in_place).then_some("attachment"),
        },
    )?;
    let mut response = StatusCode::FOUND.into_response();
    let headers = response.headers_mut();
    headers.insert(axum::http::header::LOCATION, url.parse().ok()?);
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    Some(response)
}
