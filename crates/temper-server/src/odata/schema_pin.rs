//! Stable OData error responses for scoped schema-pin mismatches.

use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_runtime::persistence::schema_deployment::SchemaExecutionPin;
use temper_runtime::tenant::TenantId;

use crate::state::ServerState;

pub(super) fn schema_pin_mismatch_response(error: &str) -> Option<axum::response::Response> {
    error
        .strip_prefix(crate::state::SCHEMA_PIN_MISMATCH_PREFIX)
        .map(str::trim)
        .map(|message| {
            crate::response::odata_error(StatusCode::CONFLICT, "SchemaPinMismatch", message)
                .into_response()
        })
}

pub(super) fn schema_pin_extraction_error_response(
    error: (StatusCode, String),
) -> axum::response::Response {
    schema_pin_mismatch_response(&error.1).unwrap_or_else(|| error.into_response())
}

pub(super) async fn resolve_scope_only_entity_pin(
    headers: &HeaderMap,
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    active_pin: Option<SchemaExecutionPin>,
) -> Result<Option<SchemaExecutionPin>, (StatusCode, String)> {
    if headers.contains_key("x-temper-schema-bundle-digest") || active_pin.is_none() {
        return Ok(active_pin);
    }
    state
        .resolve_scope_only_scoped_entity_pin(
            tenant,
            entity_type,
            entity_id,
            active_pin.expect("checked above"),
        )
        .await
        .map(Some)
        .map_err(|error| (StatusCode::CONFLICT, error))
}
