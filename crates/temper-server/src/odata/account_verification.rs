use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::Value;
use temper_runtime::tenant::TenantId;

use crate::response::{odata_error, service_unavailable_response};
use crate::state::ServerState;
use crate::state::account_verification::CommonsAccountVerificationError;

type AccountVerificationResponse = Box<axum::response::Response>;

pub(super) async fn enforce_commons_account_verified_for_write(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    fields: &Value,
) -> Result<(), AccountVerificationResponse> {
    map_account_verification_error(
        state
            .enforce_commons_verified_owner_for_write(tenant, entity_type, fields)
            .await,
    )
}

pub(super) async fn enforce_commons_account_verified_for_action(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    current_fields: &Value,
    params: &Value,
) -> Result<(), AccountVerificationResponse> {
    map_account_verification_error(
        state
            .enforce_commons_verified_owner_for_action(tenant, entity_type, current_fields, params)
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
        Err(CommonsAccountVerificationError::Internal(error)) => {
            Err(Box::new(service_unavailable_response(
                "AccountVerificationUnavailable",
                "Account verification is temporarily unavailable",
                "commons_account_verification",
                error,
            )))
        }
    }
}
