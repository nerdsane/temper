use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use temper_runtime::tenant::TenantId;

use crate::state::ServerState;

pub(super) async fn refresh_durable_tenant_policy(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Response {
    if state.policy_store().is_some()
        && let Ok(tenant) = crate::odata::extract_tenant(request.headers(), &state)
        && let Err(error) =
            crate::authz::refresh_policy_snapshot_if_stale(&state, tenant.as_str()).await
    {
        tracing::error!(tenant = %tenant, error = %error, "failed to converge durable Cedar policy");
        return unavailable_response();
    }
    next.run(request).await
}

pub(super) async fn refresh_fallback_policy(
    state: &ServerState,
    tenant_id: &TenantId,
) -> Option<Response> {
    if state.policy_store().is_some()
        && let Err(error) =
            crate::authz::refresh_policy_snapshot_if_stale(state, tenant_id.as_str()).await
    {
        tracing::error!(tenant = %tenant_id, error = %error, "failed to converge fallback Cedar policy");
        return Some(unavailable_response());
    }
    None
}

fn unavailable_response() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "authorization policy is temporarily unavailable",
    )
        .into_response()
}
