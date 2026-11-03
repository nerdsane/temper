//! Identity that the host process embedding the platform router verified.

use axum::http::HeaderMap;
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::tenant::TenantId;

/// Principal a host process verified before the platform router, e.g. from
/// a token the platform cannot interpret. Request extensions cannot be set
/// over HTTP; a host inserts this in its own middleware. It authenticates
/// the request in its requested tenant; tenant policy still authorizes it.
#[derive(Clone, Debug)]
pub struct HostVerifiedIdentity(pub SecurityContext);

impl HostVerifiedIdentity {
    /// Bind the principal to `tenant`, keeping the request's session and
    /// intent telemetry.
    pub(crate) fn into_request_context(
        self,
        tenant: TenantId,
        headers: &HeaderMap,
    ) -> AuthenticatedRequestContext {
        AuthenticatedRequestContext::new(tenant, self.0)
            .with_intent(temper_server::request_context::intent_from_headers(headers))
            .with_session_id(temper_server::request_context::session_id_from_headers(
                headers,
            ))
    }
}
