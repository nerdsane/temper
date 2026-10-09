//! Approval metadata carried only by an explicitly denied HTTP response.

use std::collections::BTreeMap;

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind};

use super::{DenialInput, record_authz_denial};
use crate::state::ServerState;

/// A denied resource check, before deciding whether the request needs approval.
///
/// This may be carried in response extensions through read/filter code. A
/// discarded row-filter denial must never be recorded as a requested action.
#[derive(Clone, Debug)]
pub struct DeniedResource {
    /// Cedar action that was denied.
    pub action: String,
    /// Cedar resource type.
    pub resource_type: String,
    /// Exact resource identifier checked by Cedar.
    pub resource_id: String,
    /// Attributes used in that check, retained for decision replay.
    pub resource_attrs: BTreeMap<String, serde_json::Value>,
    /// Original Cedar denial reason.
    pub reason: String,
}

impl DeniedResource {
    /// Record a denied interactive agent request, preserving its authenticated
    /// identity and telemetry. This never grants access or changes a policy.
    pub async fn for_request(
        self,
        state: &ServerState,
        authenticated: &AuthenticatedRequestContext,
    ) -> (StatusCode, Json<serde_json::Value>) {
        self.for_request_with_context(state, authenticated, None)
            .await
    }

    async fn for_request_with_context(
        self,
        state: &ServerState,
        authenticated: &AuthenticatedRequestContext,
        execution_ctx: Option<&crate::request_context::AgentContext>,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let security_ctx = authenticated.security_context();
        let mut body = serde_json::json!({"error": {
            "code": "AuthorizationDenied", "message": self.reason,
        }});
        let session_id = authenticated.session_id().or_else(|| {
            security_ctx
                .context_attrs
                .get("sessionId")
                .and_then(|value| value.as_str())
        });
        if security_ctx.principal.kind == PrincipalKind::Agent && session_id.is_some() {
            let decision = record_authz_denial(
                state,
                DenialInput {
                    execution_ctx,
                    tenant: authenticated.tenant().as_str(),
                    security_ctx,
                    agent_id_override: None,
                    action: &self.action,
                    resource_type: &self.resource_type,
                    resource_id: &self.resource_id,
                    resource_attrs: serde_json::json!(self.resource_attrs),
                    reason: &self.reason,
                    module_name: None,
                    from_status: None,
                    intent: authenticated.intent().map(str::to_string),
                    session_id: session_id.map(str::to_string),
                    spec_governed: Some(false),
                },
            )
            .await;
            body["error"]["message"] =
                serde_json::json!(format!("{} (decision: {})", self.reason, decision.id));
            body["decision_id"] = serde_json::json!(decision.id);
        }
        (StatusCode::FORBIDDEN, Json(body))
    }
}

/// Complete only a denial that escaped read filtering as the HTTP response.
/// Successful reads and discarded row denials cannot generate decisions here.
pub(crate) async fn resolve_requested_denial(
    state: &ServerState,
    authenticated: &AuthenticatedRequestContext,
    mut response: Response,
    execution_ctx: Option<&crate::request_context::AgentContext>,
) -> Response {
    if response.status() == StatusCode::FORBIDDEN
        && let Some(denial) = response.extensions_mut().remove::<DeniedResource>()
    {
        return denial
            .for_request_with_context(state, authenticated, execution_ctx)
            .await
            .into_response();
    }
    response
}
