//! Bearer token authentication middleware.
//!
//! Every non-health-check request must include `Authorization: Bearer <key>`.
//! The middleware resolves agent credentials first, then falls back to the
//! global `TEMPER_API_KEY` for admin/operator access.
//!
//! See ADR-0033: Platform-Assigned Agent Identity.

use crate::state::PlatformState;
use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;
use temper_runtime::tenant::TenantId;

/// Marker extension for requests that were already authenticated by an
/// outer layer and have trusted principal headers injected server-side.
#[derive(Debug, Clone, Copy)]
pub struct PreAuthenticatedRequest;

/// Axum middleware that validates Bearer token authentication and resolves
/// agent identity from credentials.
///
/// Resolution order:
/// 1. Health check paths → passthrough (no auth needed)
/// 2. No `api_token` configured → passthrough (local dev mode)
/// 3. Try agent credential resolution → if match, set `ResolvedIdentity` extension
/// 4. Try global `TEMPER_API_KEY` match → admin/operator access
/// 5. No match → 401 Unauthorized
pub async fn bearer_auth_check(
    State(state): State<PlatformState>,
    mut req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    // Allow health checks without auth (Railway probes these paths).
    if req.method() == axum::http::Method::GET
        && (req.uri().path() == "/tdata" || req.uri().path() == "/healthz")
    {
        return Ok(next.run(req).await);
    }

    // Allow identity resolution endpoint without auth — the token in the
    // request body IS the credential being resolved (self-resolving).
    if req.method() == axum::http::Method::POST && req.uri().path() == "/api/identity/resolve" {
        return Ok(next.run(req).await);
    }

    let Some(ref _expected) = state.api_token else {
        // No API key configured. The ingress edge (ADR-0157) already stripped
        // any client-asserted `x-temper-*` identity, so the request runs as an
        // anonymous Customer — never trusted/admin. Privileged surfaces deny it.
        // Passthrough keeps unprivileged local-dev/ungoverned reads working
        // without failing open into privilege.
        return Ok(next.run(req).await);
    };

    if req.extensions().get::<PreAuthenticatedRequest>().is_some()
        && req.headers().contains_key("x-temper-principal-kind")
        && req.headers().contains_key("x-temper-principal-id")
    {
        return Ok(next.run(req).await);
    }

    let Some(auth_header) = req.headers().get("authorization") else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let auth_str = auth_header.to_str().map_err(|_| StatusCode::UNAUTHORIZED)?;

    let Some(token) = auth_str.strip_prefix("Bearer ") else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let matches_global_api_key = state
        .api_token
        .as_ref()
        .is_some_and(|expected| constant_time_eq(token.as_bytes(), expected.as_bytes()));

    // Step 1: The global API key is the operator/admin credential and takes
    // precedence. The operator principal is derived here from the *resolved
    // credential* (a verified global-key match), never from a client header:
    // the edge stripped any inbound `x-temper-*`, and the resolved principal is
    // carried as a trusted `EdgeAuthenticatedPrincipal` extension that the edge
    // materializes into headers after the strip. A caller that does not hold the
    // key can neither set these headers (stripped) nor forge the extension — so
    // it can never impersonate the operator. This replaces the former
    // guest-override path, which forwarded arbitrary client principals (ARN-170).
    if matches_global_api_key {
        req.extensions_mut()
            .insert(temper_server::authz::EdgeAuthenticatedPrincipal::operator());
        return Ok(next.run(req).await);
    }

    // Step 2: Otherwise, try to resolve the token as an agent credential.
    let tenant = extract_tenant(&req);
    if let Some(identity) = state
        .identity_resolver
        .resolve(&state.server, &tenant, token)
        .await
    {
        // Agent credential resolved — inject the verified identity as a request
        // extension (never a header). Downstream `from_resolved_identity` builds
        // the Cedar principal from it.
        req.extensions_mut().insert(identity);
        return Ok(next.run(req).await);
    }

    // No match — reject.
    Err(StatusCode::UNAUTHORIZED)
}

/// Extract tenant ID from request headers, defaulting to "default".
fn extract_tenant(req: &Request) -> TenantId {
    req.headers()
        .get("x-tenant-id")
        .and_then(|v| v.to_str().ok())
        .map(TenantId::new)
        .unwrap_or_default()
}

/// Constant-time byte comparison.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::Router;
    use axum::body::Body;
    use axum::body::to_bytes;
    use axum::extract::Extension;
    use axum::http::HeaderMap;
    use axum::http::Request as HttpRequest;
    use axum::middleware;
    use axum::routing::get;
    use std::collections::BTreeMap;
    use temper_server::identity::ResolvedIdentity;
    use tower::ServiceExt;

    async fn ok_handler() -> &'static str {
        "ok"
    }

    async fn inspect_identity_handler(
        headers: HeaderMap,
        resolved_identity: Option<Extension<ResolvedIdentity>>,
    ) -> String {
        let resolved = resolved_identity
            .map(|Extension(identity)| identity.agent_type_name)
            .unwrap_or_else(|| "none".to_string());
        let principal_kind = headers
            .get("x-temper-principal-kind")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let principal_id = headers
            .get("x-temper-principal-id")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let agent_type = headers
            .get("x-temper-agent-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        format!(
            "resolved={resolved};principal_kind={principal_kind};principal_id={principal_id};agent_type={agent_type}"
        )
    }

    fn app_with_token(token: Option<String>) -> Router {
        let mut state = PlatformState::new(None);
        state.api_token = token;
        Router::new()
            .route("/tdata", get(ok_handler))
            .route("/healthz", get(ok_handler))
            .route("/tdata/Orders", get(ok_handler))
            .route("/api/specs", get(ok_handler))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                bearer_auth_check,
            ))
            .with_state(state)
    }

    #[tokio::test]
    async fn no_token_configured_passes_all() {
        let app = app_with_token(None);
        let resp = app
            .oneshot(HttpRequest::get("/api/specs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn health_check_passes_without_auth() {
        let app = app_with_token(Some("secret123".into()));
        let resp = app
            .clone()
            .oneshot(HttpRequest::get("/tdata").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp_healthz = app
            .oneshot(HttpRequest::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp_healthz.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn valid_bearer_passes() {
        let app = app_with_token(Some("secret123".into()));
        let resp = app
            .oneshot(
                HttpRequest::get("/api/specs")
                    .header("authorization", "Bearer secret123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn missing_auth_returns_401() {
        let app = app_with_token(Some("secret123".into()));
        let resp = app
            .oneshot(HttpRequest::get("/api/specs").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn wrong_token_returns_401() {
        let app = app_with_token(Some("secret123".into()));
        let resp = app
            .oneshot(
                HttpRequest::get("/api/specs")
                    .header("authorization", "Bearer wrong")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn non_bearer_scheme_returns_401() {
        let app = app_with_token(Some("secret123".into()));
        let resp = app
            .oneshot(
                HttpRequest::get("/api/specs")
                    .header("authorization", "Basic secret123")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn tdata_subpath_requires_auth() {
        let app = app_with_token(Some("secret123".into()));
        // /tdata/Orders is NOT the health check path — requires auth.
        let resp = app
            .oneshot(
                HttpRequest::get("/tdata/Orders")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn pre_authenticated_request_bypasses_bearer_requirement() {
        async fn mark_pre_authenticated(
            mut req: Request,
            next: Next,
        ) -> Result<Response, StatusCode> {
            req.extensions_mut().insert(PreAuthenticatedRequest);
            req.headers_mut()
                .insert("x-tenant-id", "default".parse().unwrap());
            req.headers_mut()
                .insert("x-temper-principal-kind", "admin".parse().unwrap());
            req.headers_mut()
                .insert("x-temper-principal-id", "dashboard-user".parse().unwrap());
            Ok(next.run(req).await)
        }

        let mut state = PlatformState::new(None);
        state.api_token = Some("secret123".into());
        let app = Router::new()
            .route("/tdata/Orders", get(ok_handler))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                bearer_auth_check,
            ))
            .layer(middleware::from_fn(mark_pre_authenticated))
            .with_state(state);

        let resp = app
            .oneshot(
                HttpRequest::get("/tdata/Orders")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// ARN-170: a global-API-key holder that also declares its own principal
    /// headers can no longer impersonate that principal. Behind the full edge
    /// (strip → bearer_auth → materialize), the client headers are stripped and
    /// the caller is the credential-derived operator Admin.
    #[tokio::test]
    async fn global_api_key_holder_becomes_operator_admin_not_client_principal() {
        let mut state = PlatformState::new(None);
        state.api_token = Some("secret123".into());
        crate::bootstrap::bootstrap_system_tenant(&state, &BTreeMap::new());
        crate::bootstrap::bootstrap_agent_specs(&state, "default", false, &BTreeMap::new());
        crate::bootstrap::bootstrap_operator_credential(&state, "secret123", "default").await;

        let app = Router::new()
            .route("/inspect", get(inspect_identity_handler))
            // Inner → outer: materialize, then auth, then strip.
            .layer(middleware::from_fn(
                temper_server::authz::materialize_authenticated_principal,
            ))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                bearer_auth_check,
            ))
            .layer(middleware::from_fn(
                temper_server::authz::strip_inbound_identity_headers,
            ))
            .with_state(state);

        let resp = app
            .oneshot(
                HttpRequest::get("/inspect")
                    .header("authorization", "Bearer secret123")
                    .header("x-tenant-id", "default")
                    // Attempt to impersonate an agent/system principal.
                    .header("x-temper-principal-kind", "agent")
                    .header("x-temper-principal-id", "system")
                    .header("x-temper-agent-type", "system")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        // The client-declared principal was stripped; the operator Admin is what
        // reaches the handler.
        assert!(body.contains("resolved=none"), "{body}");
        assert!(body.contains("principal_kind=admin"), "{body}");
        assert!(body.contains("principal_id=api-key-holder"), "{body}");
        // The impersonation attempt (agent/system) did not survive the edge.
        assert!(!body.contains("system"), "{body}");
    }

    // ── ARN-170 / ADR-0157: Class A auth-edge exploit tests ──────────────
    //
    // Each builds the production edge (strip → bearer_auth → materialize) around
    // a handler that reports the Cedar principal a real handler would derive via
    // `SecurityContext::from_headers`.

    /// Report the principal a handler derives from the (post-edge) request
    /// headers — the same `from_headers` path every real handler uses.
    async fn derived_principal(headers: HeaderMap) -> String {
        let pairs: Vec<(String, String)> = headers
            .iter()
            .filter_map(|(k, v)| {
                v.to_str()
                    .ok()
                    .map(|s| (k.as_str().to_string(), s.to_string()))
            })
            .collect();
        let ctx = temper_authz::SecurityContext::from_headers(&pairs);
        format!("{:?}:{}", ctx.principal.kind, ctx.principal.id)
    }

    fn edged_app(api_token: Option<String>) -> Router {
        let mut state = PlatformState::new(None);
        state.api_token = api_token;
        Router::new()
            .route("/whoami", get(derived_principal))
            .layer(middleware::from_fn(
                temper_server::authz::materialize_authenticated_principal,
            ))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                bearer_auth_check,
            ))
            .layer(middleware::from_fn(
                temper_server::authz::strip_inbound_identity_headers,
            ))
            .with_state(state)
    }

    async fn whoami(app: Router, req: HttpRequest<Body>) -> (StatusCode, String) {
        let resp = app.oneshot(req).await.unwrap();
        let status = resp.status();
        let body = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        (status, String::from_utf8(body.to_vec()).unwrap())
    }

    /// Exploit (a): a client-asserted `x-temper-principal-kind: admin` no longer
    /// yields Admin — even in no-key mode it derives an anonymous Customer.
    #[tokio::test]
    async fn spoofed_admin_header_does_not_yield_admin() {
        let (status, who) = whoami(
            edged_app(None),
            HttpRequest::get("/whoami")
                .header("x-temper-principal-kind", "admin")
                .header("x-temper-principal-id", "attacker")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(who, "Customer:anonymous");
    }

    /// Exploit (c): a client cannot smuggle the internal trust marker to force
    /// Admin — the edge strips it.
    #[tokio::test]
    async fn client_cannot_smuggle_trusted_marker() {
        let (status, who) = whoami(
            edged_app(None),
            HttpRequest::get("/whoami")
                .header("x-temper-principal-kind", "admin")
                .header("x-temper-principal-id", "attacker")
                .header(temper_authz::TRUSTED_PRINCIPAL_HEADER, "1")
                .header("x-temper-attr-approvallimit", "999999")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(who, "Customer:anonymous");
    }

    /// Exploit (b): with a key configured, a no-credential request (no bearer)
    /// is denied outright — fail closed.
    #[tokio::test]
    async fn no_credential_request_is_denied_when_key_configured() {
        let (status, _who) = whoami(
            edged_app(Some("secret123".into())),
            HttpRequest::get("/whoami")
                .header("x-temper-principal-kind", "admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// Positive: the real operator (holds the global key) still derives Admin —
    /// from the credential, materialized after the edge.
    #[tokio::test]
    async fn operator_key_yields_admin() {
        let (status, who) = whoami(
            edged_app(Some("secret123".into())),
            HttpRequest::get("/whoami")
                .header("authorization", "Bearer secret123")
                .header("x-tenant-id", "default")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(who, "Admin:api-key-holder");
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hell"));
    }
}
