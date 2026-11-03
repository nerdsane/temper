use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::Value;
use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;

use crate::response::odata_error;
use crate::state::ServerState;
use crate::state::account_verification::CommonsAccountVerificationError;

type AccountVerificationResponse = Box<axum::response::Response>;

/// Verify the authenticated caller before an owner-scoped commons write of
/// `fields` (create, PATCH, PUT, DELETE).
pub(super) async fn enforce_commons_account_verified_for_write(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    fields: &Value,
    security_ctx: &SecurityContext,
) -> Result<(), AccountVerificationResponse> {
    map_account_verification_error(
        state
            .enforce_commons_verified_subject(tenant, entity_type, fields, None, security_ctx)
            .await,
    )
}

/// Verify the authenticated caller before an owner-scoped bound action.
pub(super) async fn enforce_commons_account_verified_for_action(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    current_fields: &Value,
    params: &Value,
    security_ctx: &SecurityContext,
) -> Result<(), AccountVerificationResponse> {
    map_account_verification_error(
        state
            .enforce_commons_verified_subject(
                tenant,
                entity_type,
                current_fields,
                Some(params),
                security_ctx,
            )
            .await,
    )
}

fn map_account_verification_error(
    result: Result<(), CommonsAccountVerificationError>,
) -> Result<(), AccountVerificationResponse> {
    match result {
        Ok(()) => Ok(()),
        Err(CommonsAccountVerificationError::Required(required)) => Err(Box::new(
            odata_error(
                StatusCode::FORBIDDEN,
                "AccountVerificationRequired",
                &format!(
                    "Owner '{}' must be verified before writing to the commons",
                    required.owner_id
                ),
            )
            .into_response(),
        )),
        Err(CommonsAccountVerificationError::MissingOwner(owner_id)) => Err(Box::new(
            odata_error(
                StatusCode::FORBIDDEN,
                "AccountVerificationRequired",
                &format!(
                    "Owner '{owner_id}' must exist and be verified before writing to the commons"
                ),
            )
            .into_response(),
        )),
        Err(CommonsAccountVerificationError::OwnerSuspended(owner_id)) => Err(Box::new(
            odata_error(
                StatusCode::FORBIDDEN,
                "OwnerSuspended",
                &format!("Owner '{owner_id}' is suspended"),
            )
            .into_response(),
        )),
        Err(CommonsAccountVerificationError::Internal(msg)) => Err(Box::new(
            odata_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "AccountVerificationError",
                &msg,
            )
            .into_response(),
        )),
    }
}
