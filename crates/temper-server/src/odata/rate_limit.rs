use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::IntoResponse;
use serde_json::Value;
use temper_authz::{PrincipalKind, SecurityContext};
use temper_runtime::tenant::TenantId;

use crate::authz::security_context_from_headers;
use crate::identity::ResolvedIdentity;
use crate::request_context::AgentContext;
use crate::response::odata_error;
use crate::state::ServerState;

pub(crate) fn owner_id_from_fields(fields: &Value) -> Option<String> {
    first_non_empty_string(
        fields,
        &[
            "ChildOwnerId",
            "OwnerId",
            "OwnerAccountId",
            "AccountId",
            "owner_id",
            "ownerAccountId",
            "accountId",
        ],
    )
}

pub(super) fn owner_id_from_action(fields: &Value, params: &Value) -> Option<String> {
    owner_id_from_fields(params).or_else(|| owner_id_from_fields(fields))
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn enforce_commons_write_rate_limit(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    owner_id: Option<String>,
    headers: &HeaderMap,
    agent_ctx: &AgentContext,
    resolved_identity: Option<&ResolvedIdentity>,
) -> Result<(), axum::response::Response> {
    if !state.commons_guardrails_enabled(tenant) || rate_limit_exempt_entity(entity_type) {
        return Ok(());
    }

    let security_ctx = request_security_context(headers, agent_ctx, resolved_identity);
    if matches!(
        security_ctx.principal.kind,
        PrincipalKind::Admin | PrincipalKind::System
    ) {
        return Ok(());
    }

    let owner_id = owner_id.unwrap_or_else(|| security_ctx.principal.id.clone());
    if owner_id.trim().is_empty() || owner_id == "anonymous" {
        return Ok(());
    }

    match state
        .consume_commons_rate_limit_token(tenant, &owner_id, state.commons_write_action_class())
        .await
    {
        Ok(()) => Ok(()),
        Err(crate::state::rate_limit::CommonsRateLimitError::Exceeded(exceeded)) => {
            let mut response = odata_error(
                StatusCode::TOO_MANY_REQUESTS,
                "RateLimitExceeded",
                &format!(
                    "Rate limit exceeded for owner '{}' action class '{}'",
                    exceeded.owner_id, exceeded.action_class
                ),
            )
            .into_response();
            if let Some(retry_after) = exceeded.retry_after_secs
                && let Ok(value) = HeaderValue::from_str(&retry_after.to_string())
            {
                response.headers_mut().insert("Retry-After", value);
            }
            Err(response)
        }
        Err(err) => Err(odata_error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "RateLimitError",
            &err.to_string(),
        )
        .into_response()),
    }
}

fn request_security_context(
    headers: &HeaderMap,
    agent_ctx: &AgentContext,
    resolved_identity: Option<&ResolvedIdentity>,
) -> SecurityContext {
    if let Some(security) = &agent_ctx.security_ctx {
        security.clone()
    } else if let Some(identity) = resolved_identity {
        SecurityContext::from_resolved_identity(
            &identity.agent_instance_id,
            &identity.agent_type_name,
            agent_ctx.session_id.as_deref(),
        )
    } else {
        security_context_from_headers(headers, None, agent_ctx.session_id.as_deref(), None)
    }
}

fn rate_limit_exempt_entity(entity_type: &str) -> bool {
    matches!(entity_type, "Owner" | "RateLimit")
}

fn first_non_empty_string(fields: &Value, names: &[&str]) -> Option<String> {
    for name in names {
        if let Some(value) = fields.get(*name).and_then(|v| v.as_str()) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}
