use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use tracing::instrument;

use crate::odata::extract_tenant;
use crate::state::{PublishFileArtifactRequest, ServerState};

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
pub(crate) struct PublishArtifactRequest {
    file_id: String,
    label: String,
    #[serde(default)]
    owner_ref_type: String,
    #[serde(default)]
    owner_ref_id: String,
    #[serde(default)]
    source_file_version_id: String,
    #[serde(default)]
    namespace: Option<String>,
}

#[derive(Debug, serde::Serialize)]
pub(crate) struct PublishArtifactResponse {
    artifact: crate::storage::PublishedArtifactStoreRow,
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

/// POST /api/files/publish-artifact — promote a governed TemperFS file into
/// an immutable public blob and persist the public artifact record.
#[instrument(skip_all, fields(
    otel.name = "POST /api/files/publish-artifact",
    tenant = tracing::field::Empty,
    file_id = tracing::field::Empty,
    source_file_version_id = tracing::field::Empty,
    artifact_label = tracing::field::Empty,
    owner_ref_type = tracing::field::Empty,
    owner_ref_id = tracing::field::Empty,
    artifact_id = tracing::field::Empty,
    public_storage_key = tracing::field::Empty,
    public_url = tracing::field::Empty,
    byte_length = tracing::field::Empty,
    artifact_status = tracing::field::Empty,
))]
pub(crate) async fn handle_publish_artifact(
    State(state): State<ServerState>,
    headers: HeaderMap,
    axum::Json(body): axum::Json<PublishArtifactRequest>,
) -> impl IntoResponse {
    let tenant = match extract_tenant(&headers, &state) {
        Ok(t) => t,
        Err(e) => return e.into_response(),
    };
    let span = tracing::Span::current();
    span.record("tenant", tenant.as_str());
    span.record("file_id", body.file_id.as_str());
    span.record(
        "source_file_version_id",
        body.source_file_version_id.as_str(),
    );
    span.record("artifact_label", body.label.as_str());
    span.record("owner_ref_type", body.owner_ref_type.as_str());
    span.record("owner_ref_id", body.owner_ref_id.as_str());

    if body.file_id.trim().is_empty() || body.label.trim().is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "InvalidPublishArtifactRequest",
                    "message": "file_id and label are required",
                }
            })),
        )
            .into_response();
    }

    match state
        .publish_file_artifact(
            &tenant,
            PublishFileArtifactRequest {
                file_id: body.file_id,
                label: body.label,
                owner_ref_type: body.owner_ref_type,
                owner_ref_id: body.owner_ref_id,
                source_file_version_id: body.source_file_version_id,
                namespace: body.namespace,
            },
        )
        .await
    {
        Ok(artifact) => {
            span.record("artifact_id", artifact.id.as_str());
            span.record("public_storage_key", artifact.public_storage_key.as_str());
            span.record("public_url", artifact.public_url.as_str());
            span.record("byte_length", artifact.byte_length);
            span.record("artifact_status", artifact.status.as_str());
            tracing::info!(
                tenant = tenant.as_str(),
                file_id = artifact.source_file_id.as_str(),
                source_file_version_id = artifact.source_file_version_id.as_str(),
                artifact_id = artifact.id.as_str(),
                artifact_label = artifact.label.as_str(),
                public_storage_key = artifact.public_storage_key.as_str(),
                public_url = artifact.public_url.as_str(),
                byte_length = artifact.byte_length,
                artifact_status = artifact.status.as_str(),
                owner_ref_type = artifact.owner_ref_type.as_str(),
                owner_ref_id = artifact.owner_ref_id.as_str(),
                "publish artifact request completed"
            );
            (
                axum::http::StatusCode::OK,
                axum::Json(PublishArtifactResponse { artifact }),
            )
                .into_response()
        }
        Err(error) => (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            axum::Json(serde_json::json!({
                "error": {
                    "code": "PublishArtifactFailed",
                    "message": error,
                }
            })),
        )
            .into_response(),
    }
}
