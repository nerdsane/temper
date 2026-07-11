use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::Value;
use temper_runtime::tenant::TenantId;

use crate::response::{odata_error, service_unavailable_response};
use crate::state::ServerState;
use crate::state::app_uniqueness::CommonsAppUniquenessError;

pub(super) async fn enforce_commons_app_name_unique_for_write(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    fields: &Value,
) -> Result<(), axum::response::Response> {
    match state
        .enforce_commons_app_name_unique_for_write(tenant, entity_type, entity_id, fields)
        .await
    {
        Ok(()) => Ok(()),
        Err(CommonsAppUniquenessError::Conflict(conflict)) => Err(odata_error(
            StatusCode::CONFLICT,
            "AppNameAlreadyExists",
            &format!(
                "Owner '{}' already has an App named '{}' ({})",
                conflict.owner_id, conflict.name, conflict.existing_app_id
            ),
        )
        .into_response()),
        Err(CommonsAppUniquenessError::Internal(error)) => Err(service_unavailable_response(
            "AppUniquenessUnavailable",
            "App name availability is temporarily unavailable",
            "commons_app_name_uniqueness",
            error,
        )),
    }
}
