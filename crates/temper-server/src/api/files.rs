use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use tracing::instrument;

use crate::odata::extract_tenant;
use crate::state::ServerState;

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
