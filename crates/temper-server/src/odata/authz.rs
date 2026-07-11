//! Shared Cedar enforcement for OData entity operations.

use std::collections::BTreeMap;

use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;

use crate::authz::{DenialInput, record_authz_denial, security_context_from_headers};
use crate::identity::ResolvedIdentity;
use crate::request_context::AgentContext;
use crate::response::odata_error;
use crate::state::ServerState;

pub(super) const CREATE_ACTION: &str = "create";
pub(crate) const LIST_ACTION: &str = "list";
pub(crate) const READ_ACTION: &str = "read";
pub(super) const UPDATE_ACTION: &str = "update";
pub(super) const DELETE_ACTION: &str = "delete";

/// Build the authoritative Cedar principal for an external OData request.
pub(super) fn request_security_context(
    headers: &HeaderMap,
    agent_ctx: &AgentContext,
    resolved_identity: Option<&ResolvedIdentity>,
) -> SecurityContext {
    if let Some(identity) = resolved_identity {
        SecurityContext::from_resolved_identity(
            &identity.agent_instance_id,
            &identity.agent_type_name,
            agent_ctx.session_id.as_deref(),
        )
    } else {
        security_context_from_headers(headers, None, agent_ctx.session_id.as_deref(), None)
    }
}

pub(crate) fn entity_id_from_body(body: &serde_json::Value) -> Option<&str> {
    body.get("entity_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| body.get("Id").and_then(serde_json::Value::as_str))
        .or_else(|| {
            body.get("fields")
                .and_then(|fields| fields.get("Id"))
                .and_then(serde_json::Value::as_str)
        })
        .or_else(|| {
            body.get("fields")
                .and_then(|fields| fields.get("id"))
                .and_then(serde_json::Value::as_str)
        })
}

pub(crate) async fn authorize_read(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    action: &str,
    entity_type: &str,
    entity_id: &str,
    body: &serde_json::Value,
) -> Result<(), Box<Response>> {
    let fields = body.get("fields").unwrap_or(body);
    let status = body
        .get("status")
        .or_else(|| body.get("Status"))
        .or_else(|| fields.get("status"))
        .or_else(|| fields.get("Status"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let attrs = state
        .build_authz_resource_attrs(tenant, entity_type, entity_id, status, fields)
        .await
        .map_err(|error| {
            tracing::error!(
                error = %error,
                tenant = %tenant,
                entity_type,
                entity_id,
                "failed to build authoritative read authorization resource"
            );
            Box::new(
                odata_error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "ResourceStateUnavailable",
                    "Authorization resource state is temporarily unavailable",
                )
                .into_response(),
            )
        })?;
    state
        .authorize_with_context(security_ctx, action, entity_type, &attrs, tenant.as_str())
        .map_err(|denial| {
            Box::new(
                odata_error(
                    StatusCode::FORBIDDEN,
                    "AuthorizationDenied",
                    &denial.to_string(),
                )
                .into_response(),
            )
        })
}

pub(super) struct MutationResource<'a> {
    pub(super) entity_type: &'a str,
    pub(super) entity_id: &'a str,
    pub(super) attrs: &'a BTreeMap<String, serde_json::Value>,
}

pub(super) async fn authorize_mutation(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    agent_ctx: &AgentContext,
    action: &str,
    resource: MutationResource<'_>,
) -> Result<(), Response> {
    let MutationResource {
        entity_type,
        entity_id,
        attrs,
    } = resource;
    let Err(denial) =
        state.authorize_with_context(security_ctx, action, entity_type, attrs, tenant.as_str())
    else {
        return Ok(());
    };

    let reason = denial.to_string();
    let decision = record_authz_denial(
        state,
        DenialInput {
            tenant: tenant.as_str(),
            security_ctx,
            agent_id_override: agent_ctx.agent_id.as_deref(),
            action,
            resource_type: entity_type,
            resource_id: entity_id,
            resource_attrs: serde_json::to_value(attrs).unwrap_or_default(),
            reason: &reason,
            module_name: None,
            from_status: attrs
                .get("status")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        },
    )
    .await;

    Err(odata_error(
        StatusCode::FORBIDDEN,
        "AuthorizationDenied",
        &format!("{reason} (decision: {})", decision.id),
    )
    .into_response())
}
