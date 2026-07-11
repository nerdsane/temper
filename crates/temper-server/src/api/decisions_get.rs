//! Tenant-scoped decision lookup.

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use temper_authz::{AuthenticatedRequestContext, PrincipalKind};
use tracing::instrument;

use super::require_policy_auth;
use crate::authz::{require_authenticated_context, require_tenant_match};
use crate::state::{PendingDecision, ServerState};

/// GET /api/tenants/{tenant}/decisions/{id} — fetch one decision by ID.
#[instrument(skip_all, fields(tenant, id, otel.name = "GET /api/tenants/{tenant}/decisions/{id}"))]
pub(crate) async fn handle_get_decision(
    State(state): State<ServerState>,
    Path((path_tenant, id)): Path<(String, String)>,
    authenticated: Option<Extension<AuthenticatedRequestContext>>,
) -> impl IntoResponse {
    let authenticated = match require_authenticated_context(authenticated.as_deref()) {
        Ok(authenticated) => authenticated,
        Err(status) => return status.into_response(),
    };
    if let Err(status) = require_tenant_match(authenticated, &path_tenant) {
        return status.into_response();
    }
    let tenant = authenticated.tenant().as_str();
    let Some(store) = state.metadata_store_for_tenant(tenant).await else {
        tracing::error!("durable metadata backend not configured for get decision");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "durable metadata backend not configured",
        )
            .into_response();
    };
    let decision: PendingDecision = match store.get_pending_decision(tenant, &id).await {
        Ok(Some(data_str)) => match serde_json::from_str::<PendingDecision>(&data_str) {
            Ok(d) if d.tenant == tenant => d,
            _ => return (StatusCode::NOT_FOUND, "Decision not found").into_response(),
        },
        Ok(None) => return (StatusCode::NOT_FOUND, "Decision not found").into_response(),
        Err(e) => {
            tracing::error!(error = %e, backend = store.backend_name(), "failed to load decision");
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to load decision: {e}"),
            )
                .into_response();
        }
    };

    let security_ctx = authenticated.security_context();
    let owner_agent = matches!(security_ctx.principal.kind, PrincipalKind::Agent)
        && security_ctx.principal.id == decision.agent_id;
    if !owner_agent && let Some(resp) = require_policy_auth(&state, authenticated).await {
        return resp;
    }

    (StatusCode::OK, axum::Json(decision)).into_response()
}
