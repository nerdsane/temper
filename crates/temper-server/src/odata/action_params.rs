//! OData mapping for the runtime's declared action-parameter contract.

use axum::http::StatusCode;
use temper_runtime::tenant::TenantId;

use crate::entity_actor::declared_params::{ParamContractError, undeclared_param_keys};
use crate::state::ServerState;

pub(super) struct ParamValidationError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ParamValidationError {
    fn internal(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code,
            message: message.into(),
        }
    }

    fn bad_request(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message: message.into(),
        }
    }
}

/// Validate one authorized bound-action body against its transition metadata.
pub(super) fn validate_bound_action_params(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    action: &str,
    body: &serde_json::Value,
) -> Result<(), ParamValidationError> {
    let table = state
        .transition_table_for_entity(tenant, entity_type)
        .map_err(|error| ParamValidationError::internal("RegistryError", error))?
        .ok_or_else(|| {
            ParamValidationError::internal(
                "TransitionTableMissing",
                format!("no transition table registered for '{entity_type}'"),
            )
        })?;

    let undeclared = undeclared_param_keys(&table, action, body).map_err(|error| match error {
        ParamContractError::MissingMetadata(_) => {
            ParamValidationError::internal("ActionMetadataMissing", error.to_string())
        }
        ParamContractError::AmbiguousAlias { .. } => {
            ParamValidationError::bad_request("AmbiguousActionParams", error.to_string())
        }
        ParamContractError::DeclarationAliasCollision { .. } => {
            ParamValidationError::internal("ActionMetadataInvalid", error.to_string())
        }
    })?;
    if undeclared.is_empty() {
        return Ok(());
    }
    Err(ParamValidationError::bad_request(
        "UndeclaredActionParams",
        format!(
            "action '{action}' does not declare param(s): {}. Only declared params are accepted (ARN-247).",
            undeclared.join(", ")
        ),
    ))
}
