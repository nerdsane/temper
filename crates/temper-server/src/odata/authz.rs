//! Shared Cedar enforcement for OData entity operations.

use std::collections::BTreeMap;

use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use temper_authz::SecurityContext;
use temper_runtime::tenant::TenantId;

use crate::authz::{
    DenialInput, denial_status, record_denial_if_policy, security_context_from_headers,
};
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

/// Flatten an entity representation into the resource attributes supplied to Cedar.
///
/// Read responses wrap application fields below `fields`, while create payloads
/// supply fields at the top level. Supporting both shapes keeps policy
/// evaluation identical across CRUD paths.
pub(super) fn resource_attrs_from_body(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    resource_id: &str,
    body: &serde_json::Value,
) -> BTreeMap<String, serde_json::Value> {
    let mut attrs = BTreeMap::new();
    attrs.insert(
        "id".to_string(),
        serde_json::Value::String(resource_id.to_string()),
    );

    let status = body
        .get("status")
        .or_else(|| body.get("Status"))
        .or_else(|| body.get("fields").and_then(|fields| fields.get("status")))
        .or_else(|| body.get("fields").and_then(|fields| fields.get("Status")))
        .cloned()
        .unwrap_or_else(|| serde_json::Value::String(String::new()));
    attrs.insert("status".to_string(), status);

    if let Some(fields) = body.get("fields").and_then(serde_json::Value::as_object) {
        for (key, value) in fields {
            attrs.insert(key.clone(), value.clone());
        }
    } else if let Some(fields) = body.as_object() {
        for (key, value) in fields {
            if !key.starts_with('@') {
                attrs.insert(key.clone(), value.clone());
            }
        }
    }

    let has_spec = state
        .has_registered_spec(tenant, entity_type)
        .unwrap_or(false);
    attrs.insert("has_spec".to_string(), serde_json::Value::Bool(has_spec));
    attrs
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

pub(crate) fn authorize_read(
    state: &ServerState,
    tenant: &TenantId,
    security_ctx: &SecurityContext,
    action: &str,
    entity_type: &str,
    entity_id: &str,
    body: &serde_json::Value,
) -> Result<(), Box<Response>> {
    let attrs = resource_attrs_from_body(state, tenant, entity_type, entity_id, body);
    state
        .authorize_with_context(security_ctx, action, entity_type, &attrs, tenant.as_str())
        .map_err(|denial| {
            Box::new(
                odata_error(
                    denial_status(&denial),
                    denial.error_code(),
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
    let decision = record_denial_if_policy(
        state,
        &denial,
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
            intent: agent_ctx.intent.clone(),
        },
    )
    .await;

    let message = match decision {
        Some(pd) => format!("{reason} (decision: {})", pd.id),
        None => reason,
    };
    Err(odata_error(denial_status(&denial), denial.error_code(), &message).into_response())
}
