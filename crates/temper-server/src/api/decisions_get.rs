//! Tenant-scoped decision lookup.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use temper_authz::PrincipalKind;
use tracing::instrument;

use super::require_policy_auth;
use crate::authz::security_context_from_headers;
use crate::state::{PendingDecision, ServerState};

/// GET /api/tenants/{tenant}/decisions/{id} — fetch one decision by ID.
#[instrument(skip_all, fields(tenant, id, otel.name = "GET /api/tenants/{tenant}/decisions/{id}"))]
pub(crate) async fn handle_get_decision(
    State(state): State<ServerState>,
    Path((tenant, id)): Path<(String, String)>,
    headers: HeaderMap,
) -> impl IntoResponse {
    let Some(store) = state.metadata_store_for_tenant(&tenant).await else {
        tracing::error!("durable metadata backend not configured for get decision");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            "durable metadata backend not configured",
        )
            .into_response();
    };
    let decision: PendingDecision = match store.get_pending_decision(&id).await {
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

    let security_ctx = security_context_from_headers(&headers, None, None, None);
    let session_id = security_ctx
        .context_attrs
        .get("sessionId")
        .and_then(|v| v.as_str());
    let owner_agent = matches!(security_ctx.principal.kind, PrincipalKind::Agent)
        && (security_ctx.principal.id == decision.agent_id
            || session_id.is_some() && session_id == decision.session_id.as_deref());
    if !matches!(security_ctx.principal.kind, PrincipalKind::Admin)
        && !owner_agent
        && let Some(resp) = require_policy_auth(&state, &headers, &tenant).await
    {
        return resp;
    }

    (StatusCode::OK, axum::Json(decision)).into_response()
}
