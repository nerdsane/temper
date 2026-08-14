use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::Value;
use temper_runtime::tenant::TenantId;

use crate::response::odata_error;
use crate::state::ServerState;
use crate::state::storage_caps::CommonsStorageCapError;

pub(super) async fn enforce_commons_storage_cap(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    fields: &Value,
) -> Result<(), axum::response::Response> {
    match state
        .enforce_commons_storage_cap_for_write(tenant, entity_type, entity_id, action, fields)
        .await
    {
        Ok(()) => Ok(()),
        Err(error) => Err(storage_cap_error_response(error)),
    }
}

pub(super) fn storage_cap_error_response(
    error: CommonsStorageCapError,
) -> axum::response::Response {
    match error {
        CommonsStorageCapError::Exceeded(exceeded) => odata_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "StorageCapExceeded",
            &format!(
                "Storage cap exceeded for owner '{}': used {} + new {} > cap {}",
                exceeded.owner_id,
                exceeded.used_bytes,
                exceeded.additional_bytes,
                exceeded.cap_bytes
            ),
        )
        .into_response(),
        CommonsStorageCapError::ReservationCapacityExhausted => odata_error(
            StatusCode::TOO_MANY_REQUESTS,
            "StorageReservationCapacityExhausted",
            "Too many storage-reserved writes are already in flight",
        )
        .into_response(),
        CommonsStorageCapError::OwnerSuspended(owner_id) => odata_error(
            StatusCode::FORBIDDEN,
            "OwnerSuspended",
            &format!("Owner '{owner_id}' is suspended"),
        )
        .into_response(),
        CommonsStorageCapError::MissingAttribution(msg) => {
            odata_error(StatusCode::CONFLICT, "StorageAttributionMissing", &msg).into_response()
        }
        CommonsStorageCapError::Internal(msg) => {
            odata_error(StatusCode::INTERNAL_SERVER_ERROR, "StorageCapError", &msg).into_response()
        }
    }
}
