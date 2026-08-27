use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_runtime::tenant::TenantId;

use crate::blob_store::MAX_RAW_BLOB_BYTES;
use crate::response::{ODataStreamResponse, odata_error};
use crate::state::{ServerState, StreamDescriptorResolutionError};

pub(crate) async fn try_file_stream_fast_path(
    state: &ServerState,
    tenant: &TenantId,
    set_name: &str,
    entity_type: &str,
    key: &str,
) -> Option<Response> {
    if !matches!(entity_type, "File" | "FileVersion") {
        return None;
    }
    let activated = match state
        .stream_descriptor_contract_activated(tenant, None, entity_type)
        .await
    {
        Ok(activated) => activated,
        Err(error) => {
            return Some(
                odata_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    error.stable_code(),
                    "Authoritative stream descriptor fence storage is unavailable",
                )
                .into_response(),
            );
        }
    };
    if !activated {
        return None;
    }
    Some(
        match state
            .open_stream_from_descriptor(tenant, entity_type, key, MAX_RAW_BLOB_BYTES as u64)
            .await
        {
            Ok(resolved) => ODataStreamResponse {
                status: StatusCode::OK,
                body: resolved.bytes,
                content_type: resolved
                    .descriptor
                    .content_type()
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or("application/octet-stream")
                    .to_string(),
                etag: Some(resolved.descriptor.content_hash().to_string()),
            }
            .into_response(),
            Err(error @ StreamDescriptorResolutionError::Missing) => odata_error(
                StatusCode::CONFLICT,
                error.stable_code(),
                &format!("{set_name}('{key}') has no committed stream descriptor"),
            )
            .into_response(),
            Err(error @ StreamDescriptorResolutionError::BudgetExceeded) => odata_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                error.stable_code(),
                "Committed stream exceeds the platform read budget",
            )
            .into_response(),
            Err(error @ StreamDescriptorResolutionError::Integrity(_)) => odata_error(
                StatusCode::CONFLICT,
                error.stable_code(),
                &error.to_string(),
            )
            .into_response(),
            Err(error @ StreamDescriptorResolutionError::Consistency(_)) => odata_error(
                StatusCode::CONFLICT,
                error.stable_code(),
                &error.to_string(),
            )
            .into_response(),
            Err(error @ StreamDescriptorResolutionError::ReplayBudgetExceeded) => odata_error(
                StatusCode::CONFLICT,
                error.stable_code(),
                "Stream descriptor replay exceeded its event budget",
            )
            .into_response(),
            Err(error @ StreamDescriptorResolutionError::JournalUnavailable)
            | Err(error @ StreamDescriptorResolutionError::Storage(_)) => odata_error(
                StatusCode::SERVICE_UNAVAILABLE,
                error.stable_code(),
                "Authoritative stream descriptor storage is unavailable",
            )
            .into_response(),
        },
    )
}
