//! Platform router construction.
//!
//! Assembles the full axum router with tenant-aware routing.
//! The API is the **Temper Data API** at `/tdata`.

use axum::Router;
use axum::http::StatusCode;
use axum::middleware;
use axum::routing;

use crate::bearer_auth::bearer_auth_check;
use crate::state::PlatformState;
use crate::tenant_access::tenant_access_check;

/// Build the full platform router.
///
/// Route structure:
/// - `GET  /tdata`            → service document (tenant-scoped)
/// - `GET  /tdata/$metadata`  → CSDL XML (tenant-scoped)
/// - `GET  /tdata/$hints`     → agent hints JSON
/// - `GET  /tdata/{*path}`    → entity set / entity / navigation / function
/// - `POST /tdata/{*path}`    → create entity / bound action
///
/// Tenant is extracted from the `X-Tenant-Id` header. Falls back to the
/// first registered tenant in the SpecRegistry.
pub fn build_platform_router(state: PlatformState) -> Router {
    let tenant_api = crate::tenant_api::tenant_api_router();
    let health = Router::new().route("/healthz", routing::get(|| async { StatusCode::OK }));

    // Platform observe routes — merged at /observe/* to avoid the /api double-nest
    // collision between temper-server's /api routes and the platform's /api routes.
    let platform_observe = Router::new()
        .route(
            "/observe/os-apps",
            routing::get(crate::tenant_api::list_os_apps),
        )
        .route(
            "/observe/os-apps/{name}",
            routing::get(crate::tenant_api::get_os_app_guide),
        );

    // Identity resolution endpoint — used by MCP server at startup.
    let identity_api = Router::new().route(
        "/api/identity/resolve",
        routing::post(temper_server::identity::endpoint::handle_identity_resolve),
    );

    temper_server::build_router(state.server.clone())
        .merge(health)
        .merge(identity_api.with_state(state.server.clone()))
        .merge(platform_observe.with_state(state.clone()))
        .nest("/api", tenant_api.with_state(state.clone()))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            tenant_access_check,
        ))
        .layer(middleware::from_fn_with_state(state, bearer_auth_check))
        // Defense in depth: raw authority headers are removed before tenant
        // credential resolution creates a typed request context (ADR-0157).
        .layer(middleware::from_fn(
            temper_server::authz::strip_inbound_identity_headers,
        ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Method, Request, StatusCode};
    use temper_authz::{AuthenticatedRequestContext, SecurityContext};
    use temper_runtime::tenant::TenantId;
    use tower::ServiceExt;

    const ROUTER_TEST_PRINCIPAL: &str = "router-test-agent";

    fn test_state() -> PlatformState {
        PlatformState::new(None)
    }

    fn authenticated_request(
        state: &PlatformState,
        method: Method,
        uri: &str,
        body: Body,
    ) -> Request<Body> {
        let credential = state
            .server
            .internal_invocation_credentials
            .issue_for_url(
                AuthenticatedRequestContext::new(
                    TenantId::default(),
                    SecurityContext::from_resolved_identity(
                        ROUTER_TEST_PRINCIPAL,
                        "operator",
                        None,
                    ),
                ),
                method.as_str(),
                &format!("http://127.0.0.1:3000{uri}"),
            )
            .expect("test credential should issue");

        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {credential}"))
            .header("x-tenant-id", "default")
            .body(body)
            .expect("test request should build")
    }

    fn allow_app_catalog(state: &PlatformState) {
        state
            .server
            .authz
            .reload_tenant_policies(
                "default",
                &format!(
                    r#"
permit(
  principal == Agent::"{ROUTER_TEST_PRINCIPAL}",
  action == Action::"read_app_catalog",
  resource == AppCatalog::"all"
);
"#
                ),
            )
            .expect("test catalog policy should parse");
    }

    #[tokio::test]
    async fn test_tdata_routes_accessible() {
        let app = build_platform_router(test_state());
        let response = app
            .oneshot(
                Request::get("/tdata")
                    .header("X-Tenant-Id", "default")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_unknown_route_returns_404() {
        let state = test_state();
        let request = authenticated_request(&state, Method::GET, "/nonexistent", Body::empty());
        let app = build_platform_router(state);
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_healthz_route_returns_200() {
        let app = build_platform_router(test_state());
        let response = app
            .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn identity_resolve_rejects_malformed_body_tenant_without_panicking() {
        let app = build_platform_router(test_state());
        let response = app
            .oneshot(
                Request::post("/api/identity/resolve")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"bearer_token":"x","tenant":":"}"#))
                    .unwrap(),
            )
            .await
            .expect("malformed tenant must produce an HTTP response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn test_old_routes_gone() {
        let state = test_state();
        let app = build_platform_router(state.clone());

        // /dev, /prod, and /odata should not exist
        for path in &["/dev", "/prod", "/odata"] {
            let request = authenticated_request(&state, Method::GET, path, Body::empty());
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{path} should be 404"
            );
        }
    }

    // ── OS App Catalog Integration Tests ──────────────────────────

    #[tokio::test]
    async fn test_get_os_apps_returns_200() {
        let state = test_state();
        allow_app_catalog(&state);
        let request = authenticated_request(&state, Method::GET, "/api/os-apps", Body::empty());
        let app = build_platform_router(state);
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let apps = json["apps"].as_array().unwrap();
        assert!(!apps.is_empty());
        // Verify a known app is present (order depends on filesystem scan).
        let names: Vec<&str> = apps.iter().filter_map(|a| a["name"].as_str()).collect();
        assert!(
            names.contains(&"project-management"),
            "missing project-management: {names:?}"
        );
    }

    #[tokio::test]
    async fn test_local_os_app_install_route_is_removed() {
        let state = test_state();
        let request = authenticated_request(
            &state,
            Method::POST,
            "/api/os-apps/project-management/install",
            Body::from(r#"{"tenant":"test-install"}"#),
        );
        let app = build_platform_router(state);
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_observe_os_apps_returns_200() {
        let state = test_state();
        allow_app_catalog(&state);
        let request = authenticated_request(&state, Method::GET, "/observe/os-apps", Body::empty());
        let app = build_platform_router(state);
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let apps = json["apps"].as_array().unwrap();
        assert!(!apps.is_empty());
        // Verify a known app is present (order depends on filesystem scan).
        let names: Vec<&str> = apps.iter().filter_map(|a| a["name"].as_str()).collect();
        assert!(
            names.contains(&"project-management"),
            "missing project-management: {names:?}"
        );
    }

    #[tokio::test]
    async fn test_observe_local_os_app_install_route_is_removed() {
        let state = test_state();
        let request = authenticated_request(
            &state,
            Method::POST,
            "/observe/os-apps/project-management/install",
            Body::from(r#"{"tenant":"test"}"#),
        );
        let app = build_platform_router(state);
        let response = app.oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
