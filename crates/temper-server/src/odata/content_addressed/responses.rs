//! Error and metadata-only response helpers for raw Blob ingest.

use axum::http::StatusCode;
use axum::response::IntoResponse as _;

use crate::blob_store::BlobStageError;
use crate::response::odata_error;

pub(super) fn stage_error_response(error: BlobStageError) -> axum::response::Response {
    let (status, code) = match &error {
        BlobStageError::BodyStream(_) => (StatusCode::BAD_REQUEST, "BodyStreamError"),
        BlobStageError::BodyExceedsDeclaredLength { .. } => {
            (StatusCode::BAD_REQUEST, "BodyExceedsContentLength")
        }
        BlobStageError::BodyShorterThanDeclaredLength { .. } => {
            (StatusCode::BAD_REQUEST, "BodyShorterThanContentLength")
        }
        BlobStageError::IdleTimeout { .. } => {
            (StatusCode::REQUEST_TIMEOUT, "BlobIngestIdleTimeout")
        }
        BlobStageError::TotalDeadline { .. } => {
            (StatusCode::REQUEST_TIMEOUT, "BlobIngestDeadlineExceeded")
        }
        BlobStageError::ThroughputTooLow { .. } => {
            (StatusCode::REQUEST_TIMEOUT, "BlobIngestThroughputTooLow")
        }
        BlobStageError::StagingBudgetExhausted { .. } => (
            StatusCode::TOO_MANY_REQUESTS,
            "BlobIngestStagingBudgetExhausted",
        ),
        BlobStageError::Storage(_) => (StatusCode::INSUFFICIENT_STORAGE, "BlobStagingFailed"),
    };
    let message = if matches!(&error, BlobStageError::Storage(_)) {
        tracing::error!(%error, "raw Blob staging failed");
        "Blob staging failed".to_string()
    } else {
        error.to_string()
    };
    odata_error(status, code, &message).into_response()
}

pub(super) fn blob_store_error_response(error: &str) -> axum::response::Response {
    tracing::error!(%error, "raw Blob object-store write failed");
    odata_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "BlobStoreWriteFailed",
        "Blob object-store write failed",
    )
    .into_response()
}

pub(super) fn remove_binary_fields_from_create_response(state: &mut serde_json::Value) {
    if let Some(fields) = state
        .get_mut("fields")
        .and_then(serde_json::Value::as_object_mut)
    {
        fields.remove("Content");
        fields.remove("CanonicalBytes");
    }
}
