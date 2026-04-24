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
        // No API key configured — passthrough (local dev mode).
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

    let has_explicit_principal = req.headers().contains_key("x-temper-principal-kind")
        && req.headers().contains_key("x-temper-principal-id");
    let matches_global_api_key = state
        .api_token
        .as_ref()
        .is_some_and(|expected| constant_time_eq(token.as_bytes(), expected.as_bytes()));

    // ADR-0043 guest override path: internal loopback callers may present the
    // platform API key while explicitly declaring the principal they are acting
    // as. Preserve those headers instead of collapsing the request into the
    // bootstrapped operator credential.
    if matches_global_api_key && has_explicit_principal {
        return Ok(next.run(req).await);
    }

    // Step 1: Try to resolve as an agent credential.
    let tenant = extract_tenant(&req);
    if let Some(identity) = state
        .identity_resolver
        .resolve(&state.server, &tenant, token)
        .await
    {
        // Agent credential resolved — inject into request extensions.
        req.extensions_mut().insert(identity);
        return Ok(next.run(req).await);
    }

    // Step 2: Fall back to global API key (admin/operator access).
    if matches_global_api_key {
        if !req.headers().contains_key("x-temper-principal-kind") {
            req.headers_mut().insert(
                "x-temper-principal-kind",
                "admin"
                    .parse()
                    .expect("valid x-temper-principal-kind header"),
            );
        }
        if !req.headers().contains_key("x-temper-principal-id") {
            req.headers_mut().insert(
                "x-temper-principal-id",
                "api-key-holder"
                    .parse()
                    .expect("valid x-temper-principal-id header"),
            );
        }
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
    use axum::body::to_bytes;
    use axum::body::Body;
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

    #[tokio::test]
    async fn global_api_key_preserves_explicit_principal_headers() {
        let mut state = PlatformState::new(None);
        state.api_token = Some("secret123".into());
        crate::bootstrap::bootstrap_system_tenant(&state, &BTreeMap::new());
        crate::bootstrap::bootstrap_agent_specs(&state, "default", false, &BTreeMap::new());
        crate::bootstrap::bootstrap_operator_credential(&state, "secret123", "default").await;

        let app = Router::new()
            .route("/inspect", get(inspect_identity_handler))
            .layer(middleware::from_fn_with_state(
                state.clone(),
                bearer_auth_check,
            ))
            .with_state(state);

        let resp = app
            .oneshot(
                HttpRequest::get("/inspect")
                    .header("authorization", "Bearer secret123")
                    .header("x-tenant-id", "default")
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
        assert!(
            body.contains("resolved=none"),
            "global API key with explicit principal headers must not inject operator identity: {body}"
        );
        assert!(body.contains("principal_kind=agent"));
        assert!(body.contains("principal_id=system"));
        assert!(body.contains("agent_type=system"));
    }

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hell"));
    }
}
