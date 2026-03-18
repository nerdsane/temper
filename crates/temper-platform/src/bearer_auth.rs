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

    let Some(auth_header) = req.headers().get("authorization") else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let auth_str = auth_header.to_str().map_err(|_| StatusCode::UNAUTHORIZED)?;

    let Some(token) = auth_str.strip_prefix("Bearer ") else {
        return Err(StatusCode::UNAUTHORIZED);
    };

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
    if let Some(ref expected) = state.api_token
        && constant_time_eq(token.as_bytes(), expected.as_bytes())
    {
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
    use axum::http::Request as HttpRequest;
    use axum::middleware;
    use axum::routing::get;
    use tower::ServiceExt;

    async fn ok_handler() -> &'static str {
        "ok"
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

    #[test]
    fn constant_time_eq_works() {
        assert!(constant_time_eq(b"hello", b"hello"));
        assert!(!constant_time_eq(b"hello", b"world"));
        assert!(!constant_time_eq(b"hello", b"hell"));
    }
}
