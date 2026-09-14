//! Shared Agent Runtime handler helpers.

use axum::Json;
use axum::extract::Extension;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::{IntoResponse, Response};
use temper_authz::AuthenticatedRequestContext;
use temper_runtime::tenant::TenantId;

use crate::agent_runtime::models::ErrorResponse;
use crate::request_context::AgentContext;
use crate::state::ServerState;

/// Entity type served by the app-installed agent runtime façade.
pub(super) const AGENT_ENTITY_TYPE: &str = "TemperAgent";

/// Require typed authenticated authority resolved by the platform bearer edge.
///
/// Identity is never reconstructed from request headers: the platform's
/// `bearer_auth_check` resolves `Authorization: Bearer <token>` against the
/// tenant's `AgentCredential` registry and attaches the typed context. A
/// missing context means the caller presented no resolvable credential.
pub(super) fn require_auth(
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> Result<(TenantId, AuthenticatedRequestContext), Box<Response>> {
    match authenticated {
        Some(Extension(ctx)) => Ok((ctx.tenant().clone(), ctx)),
        None => Err(Box::new(error_response(
            StatusCode::UNAUTHORIZED,
            "a valid tenant credential is required (Authorization: Bearer <token>)",
        ))),
    }
}

/// Require the agent-runtime app contract to be installed for this tenant.
///
/// The `/v1/agent-runs` surface is an app façade over `TemperAgent`, not a
/// provider-neutral kernel primitive. Keep requests fail-closed unless the
/// app's governed IOA spec is registered for the caller's tenant.
pub(super) fn require_agent_app_contract(
    state: &ServerState,
    tenant: &TenantId,
) -> Result<(), Box<Response>> {
    match state.has_registered_spec(tenant, AGENT_ENTITY_TYPE) {
        Ok(true) => Ok(()),
        Ok(false) => Err(Box::new(error_response(
            StatusCode::NOT_FOUND,
            "agent runtime app contract is not installed for this tenant",
        ))),
        Err(error) => Err(Box::new(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            &error,
        ))),
    }
}

/// Build a dispatch context that carries the caller's exact authority.
///
/// Mirrors the OData write path: the security context is attached verbatim and
/// agent identity fields are copied only for Agent/Admin principals. No field
/// is reconstructed from request headers.
pub(super) fn caller_agent_context(authenticated: &AuthenticatedRequestContext) -> AgentContext {
    let security_context = authenticated.security_context();
    let mut agent_ctx = AgentContext {
        security_ctx: Some(security_context.clone()),
        ..AgentContext::default()
    };
    if matches!(
        security_context.principal.kind,
        temper_authz::PrincipalKind::Agent | temper_authz::PrincipalKind::Admin
    ) {
        agent_ctx.agent_id = Some(security_context.principal.id.clone());
        agent_ctx.agent_type = security_context.principal.agent_type.clone();
    }
    agent_ctx
}

/// Build a JSON error response.
pub(super) fn error_response(status: StatusCode, message: &str) -> Response {
    (
        status,
        [(CONTENT_TYPE, "application/json")],
        Json(ErrorResponse {
            error: message.to_string(),
        }),
    )
        .into_response()
}
