//! Internal blob storage endpoint for TemperFS.
//!
//! Provides `PUT/GET /_internal/blobs/{*path}` backed by Turso.
//! The blob_adapter WASM module uploads/downloads through these endpoints
//! when no external blob storage (R2/S3) is configured.

use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;

use crate::state::ServerState;

/// `PUT /_internal/blobs/{*path}` — store a blob.
pub async fn put_blob(
    State(state): State<ServerState>,
    Path(path): Path<String>,
    body: Bytes,
) -> impl IntoResponse {
    let Some(store) = state.platform_persistent_store().cloned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Blob storage requires Turso".to_string(),
        )
            .into_response();
    };

    match store.put_blob(&path, &body).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => {
            tracing::error!(error = %e, path = %path, "blob put failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}

/// `GET /_internal/blobs/{*path}` — retrieve a blob.
pub async fn get_blob(
    State(state): State<ServerState>,
    Path(path): Path<String>,
) -> impl IntoResponse {
    let Some(store) = state.platform_persistent_store().cloned() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "Blob storage requires Turso".to_string(),
        )
            .into_response();
    };

    match store.get_blob(&path).await {
        Ok(Some(data)) => (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/octet-stream")],
            data,
        )
            .into_response(),
        Ok(None) => StatusCode::NOT_FOUND.into_response(),
        Err(e) => {
            tracing::error!(error = %e, path = %path, "blob get failed");
            (StatusCode::INTERNAL_SERVER_ERROR, e).into_response()
        }
    }
}
