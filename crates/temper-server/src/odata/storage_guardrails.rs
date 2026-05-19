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
        Err(CommonsStorageCapError::Exceeded(exceeded)) => Err(odata_error(
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
        .into_response()),
        Err(CommonsStorageCapError::OwnerSuspended(owner_id)) => Err(odata_error(
            StatusCode::FORBIDDEN,
            "OwnerSuspended",
            &format!("Owner '{owner_id}' is suspended"),
        )
        .into_response()),
        Err(CommonsStorageCapError::MissingAttribution(msg)) => {
            Err(
                odata_error(StatusCode::CONFLICT, "StorageAttributionMissing", &msg)
                    .into_response(),
            )
        }
        Err(CommonsStorageCapError::Internal(msg)) => {
            Err(
                odata_error(StatusCode::INTERNAL_SERVER_ERROR, "StorageCapError", &msg)
                    .into_response(),
            )
        }
    }
}
