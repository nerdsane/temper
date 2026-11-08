//! Answers for a tenant the server cannot serve yet, or at all.
//!
//! A tenant still starting (restored after the server serves, see
//! [`crate::tenant_restore`]) holds its requests until its restore attempt
//! ends, and answers 503 `TenantStarting` if that attempt failed. A tenant
//! whose stored specs did not load is left out of the registry so the others
//! are served; its requests get 503 `TenantUnavailable`, not the 404 an
//! unknown entity set would give, so the tenant and its operator can see it.
//! That reason is in the server log: it can quote the tenant's stored rows.

use std::time::Duration;

use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use temper_authz::AuthenticatedRequestContext;

use super::common::extract_tenant;
use crate::response::odata_error;
use crate::state::ServerState;
use crate::tenant_restore::Standing;
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

/// The answer for a request acting for `tenant` when the tenant cannot be
/// served: a starting tenant is waited for first, then answers 503
/// `TenantStarting` if its restore attempt failed; a tenant startup could not
/// restore answers 503 `TenantUnavailable`. None: serve the request.
///
/// The platform's credential check asks this before it looks a credential
/// up: neither tenant has its credential records loaded, so a lookup would
/// report a valid credential as invalid.
pub async fn tenant_not_ready_response(state: &ServerState, tenant: &TenantId) -> Option<Response> {
    match state.tenant_restore.wait_for(tenant).await {
        Standing::Restored => unavailable_tenant_response(state, tenant),
        Standing::Starting {
            reason,
            retry_after,
        } => Some(tenant_starting_response(tenant, &reason, retry_after)),
    }
}

/// 503 `TenantStarting`: `tenant`'s restore after serving could not read
/// its store, for `reason`; it is tried again within `retry_after`.
fn tenant_starting_response(tenant: &TenantId, reason: &str, retry_after: Duration) -> Response {
    let mut response = odata_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "TenantStarting",
        &format!("Tenant '{tenant}' is starting: its restore could not finish ({reason})."),
    )
    .into_response();
    let seconds = retry_after.as_secs().max(1).to_string();
    response.headers_mut().insert(
        header::RETRY_AFTER,
        HeaderValue::from_str(&seconds).expect("a number is a header value"),
    );
    response
}

/// Hold or refuse `/tdata` requests for a tenant that cannot be served yet,
/// or at all ([`tenant_not_ready_response`]).
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
    if let Some(tenant) = tenant
        && let Some(response) = tenant_not_ready_response(&state, &tenant).await
    {
        return response;
    }
    next.run(request).await
}
