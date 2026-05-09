use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use tracing::instrument;

use crate::odata::extract_tenant;
use crate::state::{PublishFileAssetRequest, ServerState};

#[derive(Debug, serde::Deserialize)]
pub(crate) struct BatchTextFileReadRequest {
    #[serde(default)]
    file_ids: Vec<String>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct BatchTextFileVersionReadRequest {
    #[serde(default)]
    file_version_ids: Vec<String>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct BatchTextFileReadResponse {
    files: Vec<crate::state::TextFileReadResult>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct BatchTextFileVersionReadResponse {
    files: Vec<crate::state::TextFileVersionReadResult>,
}

#[derive(Debug, serde::Deserialize)]
pub(crate) struct PublishAssetRequest {
    file_id: String,
    kind: String,
    #[serde(default)]
    owner_entity_type: String,
    #[serde(default)]
    owner_entity_id: String,
    #[serde(default)]
    source_file_version_id: String,
    #[serde(default)]
    public_key_prefix: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct PublishAssetResponse {
    asset: temper_store_turso::PublishedAssetRow,
}

/// POST /api/files/read-text-batch — read many text file bodies via the
/// projection-backed immutable content path.
#[instrument(skip_all, fields(otel.name = "POST /api/files/read-text-batch"))]
pub(crate) async fn handle_read_text_batch(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<BatchTextFileReadRequest>,
) -> impl IntoResponse {
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };

    match state.read_file_texts_batch(&tenant, &body.file_ids).await {
        Ok(files) => (
            axum::http::StatusCode::OK,
            axum::Json(BatchTextFileReadResponse { files }),
        )
            .into_response(),
        Err(error) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "BatchFileReadFailed",
                    "message": error,
                }
            })),
        )
            .into_response(),
    }
}

/// POST /api/files/read-version-text-batch — read many immutable file version
/// bodies via projections + direct blob fetch.
#[instrument(skip_all, fields(otel.name = "POST /api/files/read-version-text-batch"))]
pub(crate) async fn handle_read_version_text_batch(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<BatchTextFileVersionReadRequest>,
) -> impl IntoResponse {
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };

    match state
        .read_file_version_texts_batch(&tenant, &body.file_version_ids)
        .await
    {
        Ok(files) => (
            axum::http::StatusCode::OK,
            axum::Json(BatchTextFileVersionReadResponse { files }),
        )
            .into_response(),
        Err(error) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "BatchFileVersionReadFailed",
                    "message": error,
                }
            })),
        )
            .into_response(),
    }
}

/// POST /api/files/publish-asset — promote a governed TemperFS file into an
/// immutable public blob and persist the public asset record.
#[instrument(skip_all, fields(otel.name = "POST /api/files/publish-asset"))]
pub(crate) async fn handle_publish_asset(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<PublishAssetRequest>,
) -> impl IntoResponse {
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };

    if body.file_id.trim().is_empty() || body.kind.trim().is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "InvalidPublishAssetRequest",
                    "message": "file_id and kind are required",
                }
            })),
        )
            .into_response();
    }

    match state
        .publish_file_asset(
            &tenant,
            PublishFileAssetRequest {
                file_id: body.file_id,
                kind: body.kind,
                owner_entity_type: body.owner_entity_type,
                owner_entity_id: body.owner_entity_id,
                source_file_version_id: body.source_file_version_id,
                public_key_prefix: body.public_key_prefix,
            },
        )
        .await
    {
        Ok(asset) => (
            axum::http::StatusCode::OK,
            axum::Json(PublishAssetResponse { asset }),
        )
            .into_response(),
        Err(error) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "PublishAssetFailed",
                    "message": error,
                }
            })),
        )
            .into_response(),
    }
}
