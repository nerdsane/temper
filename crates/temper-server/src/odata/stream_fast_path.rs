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
