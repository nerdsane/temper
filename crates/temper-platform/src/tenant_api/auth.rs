use std::collections::BTreeMap;

use axum::http::StatusCode;
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::tenant::TenantId;

use crate::state::PlatformState;

pub(super) fn require_authenticated(
    authenticated: Option<&AuthenticatedRequestContext>,
) -> Result<&AuthenticatedRequestContext, StatusCode> {
    authenticated.ok_or(StatusCode::UNAUTHORIZED)
}

pub(super) fn validate_tenant_id(tenant: &str) -> Result<TenantId, StatusCode> {
    TenantId::try_new(tenant).map_err(|_| StatusCode::BAD_REQUEST)
}

pub(super) fn require_same_tenant(
    authenticated: &AuthenticatedRequestContext,
    target_tenant: &str,
) -> Result<(), StatusCode> {
    validate_tenant_id(target_tenant)?;
    if authenticated.tenant().as_str() != target_tenant {
        return Err(StatusCode::FORBIDDEN);
    }
    Ok(())
}

pub(super) fn require_control_plane(
    authenticated: &AuthenticatedRequestContext,
) -> Result<(), StatusCode> {
    if authenticated.tenant().as_str() == "default" {
        return Ok(());
    }
    Err(StatusCode::FORBIDDEN)
}

pub(super) struct PlatformResourceAuthorization<'a> {
    pub action: &'a str,
    pub resource_type: &'a str,
    pub resource_id: &'a str,
    pub attrs: BTreeMap<String, serde_json::Value>,
}

pub(super) fn require_resource_authorization(
    state: &PlatformState,
    authenticated: &AuthenticatedRequestContext,
    mut input: PlatformResourceAuthorization<'_>,
) -> Result<(), StatusCode> {
    input.attrs.insert(
        "id".to_string(),
        serde_json::Value::String(input.resource_id.to_string()),
    );
    input.attrs.insert(
        "credentialTenant".to_string(),
        serde_json::Value::String(authenticated.tenant().to_string()),
    );
    state
        .server
        .authorize_with_context(
            authenticated.security_context(),
            input.action,
            input.resource_type,
            &input.attrs,
            authenticated.tenant().as_str(),
        )
        .map_err(|denial| {
            tracing::warn!(
                reason = %denial,
                tenant = %authenticated.tenant(),
                principal_id = %authenticated.security_context().principal.id,
                action = input.action,
                resource_type = input.resource_type,
                resource_id = input.resource_id,
                "platform management operation denied"
            );
            StatusCode::FORBIDDEN
        })
}
