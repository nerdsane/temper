//! Bearer token authentication middleware.
//!
//! When `TEMPER_API_KEY` is configured, all HTTP requests must include
//! `Authorization: Bearer <key>`. When not configured, all requests
//! pass through (backward compatibility for local development).

use axum::extract::{Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::Response;

use crate::state::PlatformState;

/// Axum middleware that validates Bearer token authentication.
///
/// Behavior:
/// - No `api_token` configured → passthrough (local dev backward compat)
/// - Health check path `/tdata` exact → passthrough (Railway healthcheck)
/// - Valid `Authorization: Bearer <token>` → passthrough
/// - Missing or invalid token → 401 Unauthorized
pub async fn bearer_auth_check(
    State(state): State<PlatformState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let Some(ref expected) = state.api_token else {
        // No API key configured — passthrough.
        return Ok(next.run(req).await);
    };

    // Allow health check without auth (Railway probes this).
    if req.uri().path() == "/tdata" && req.method() == axum::http::Method::GET {
        return Ok(next.run(req).await);
    }

    let Some(auth_header) = req.headers().get("authorization") else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    let auth_str = auth_header.to_str().map_err(|_| StatusCode::UNAUTHORIZED)?;

    let Some(token) = auth_str.strip_prefix("Bearer ") else {
        return Err(StatusCode::UNAUTHORIZED);
    };

    // Constant-time comparison to prevent timing attacks.
    if constant_time_eq(token.as_bytes(), expected.as_bytes()) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
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
            .oneshot(HttpRequest::get("/tdata").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
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
