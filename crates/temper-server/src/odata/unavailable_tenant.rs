//! Answer for a tenant whose stored specs did not load.
//!
//! Startup leaves such a tenant out of the registry so the others are served.
//! Its own requests get a 503 naming that state, not the 404 an unknown
//! entity set would give, so the tenant and its operator can see it. The
//! reason itself is in the server log: it can quote the tenant's stored rows.

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use temper_authz::AuthenticatedRequestContext;

use super::common::extract_tenant;
use crate::response::odata_error;
use crate::state::ServerState;
use temper_runtime::tenant::TenantId;

impl ServerState {
    /// Why `tenant` is unavailable, if startup could not restore its stored
    /// specs. Such a tenant's stored rows are kept as they are for repair.
    pub fn tenant_unavailable_reason(&self, tenant: &TenantId) -> Option<String> {
        self.registry
            .read()
            .expect("registry lock poisoned")
            .tenant_unavailable_reason(tenant)
            .map(str::to_string)
    }
}

/// The 503 for a request acting for `tenant`, if startup left it unavailable.
///
/// The platform's credential check answers with it before it looks a
/// credential up: an unavailable tenant has no credential records loaded, so
/// a lookup would report a valid credential as invalid.
pub fn unavailable_tenant_response(state: &ServerState, tenant: &TenantId) -> Option<Response> {
    // A poisoned registry is not this tenant's state: the credential lookup
    // after this reports it as the dependency failure it is (503).
    let registry = state.registry.read().ok()?;
    registry.tenant_unavailable_reason(tenant)?;
    drop(registry);
    Some(
        odata_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "TenantUnavailable",
            &format!(
                "Tenant '{tenant}' is unavailable: its stored specs did not load at startup. \
                 The server log names the row; fix it and restart."
            ),
        )
        .into_response(),
    )
}

/// Refuse `/tdata` requests for a tenant marked unavailable at startup.
///
/// The tenant is the one each handler would act for: the credential's tenant
/// when the request is authenticated, otherwise the `X-Tenant-Id` header.
pub(crate) async fn refuse_unavailable_tenant(
    State(state): State<ServerState>,
    request: Request,
    next: Next,
) -> Response {
    let tenant = match request.extensions().get::<AuthenticatedRequestContext>() {
        Some(context) => Some(context.tenant().clone()),
        None => extract_tenant(request.headers(), &state).ok(),
    };
    if let Some(response) = tenant.and_then(|tenant| unavailable_tenant_response(&state, &tenant)) {
        return response;
    }
    next.run(request).await
}
