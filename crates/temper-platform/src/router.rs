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
            "/observe/os-apps/{name}/install",
            routing::post(crate::tenant_api::install_os_app),
        )
        .route(
            "/observe/tenants/{id}",
            routing::delete(crate::tenant_api::delete_tenant),
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
            crate::identity_cache::invalidate_identity_cache_on_credential_mutation,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            tenant_access_check,
        ))
        .layer(middleware::from_fn_with_state(state, bearer_auth_check))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state() -> PlatformState {
        PlatformState::new(None)
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
        let app = build_platform_router(test_state());
        let response = app
            .oneshot(Request::get("/nonexistent").body(Body::empty()).unwrap())
            .await
            .unwrap();

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
    async fn test_old_routes_gone() {
        let app = build_platform_router(test_state());

        // /dev, /prod, and /odata should not exist
        for path in &["/dev", "/prod", "/odata"] {
            let response = app
                .clone()
                .oneshot(Request::get(*path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{path} should be 404"
            );
        }
    }

    // ── OS App Catalog Integration Tests ───────────────────────────

    #[tokio::test]
    async fn test_get_os_apps_returns_200() {
        let app = build_platform_router(test_state());
        let response = app
            .oneshot(Request::get("/api/os-apps").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let apps = json["apps"].as_array().unwrap();
        assert!(!apps.is_empty());
        assert_eq!(apps[0]["name"], "project-management");
    }

    #[tokio::test]
    async fn test_install_os_app_project_management() {
        let app = build_platform_router(test_state());
        let response = app
            .oneshot(
                Request::post("/api/os-apps/project-management/install")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"tenant":"test-install"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "installed");
        // Fresh install — all 5 PM specs should be added.
        let added = json["added"].as_array().unwrap();
        assert_eq!(added.len(), 5);
        assert!(json["updated"].as_array().unwrap().is_empty());
        assert!(json["skipped"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_get_observe_os_apps_returns_200() {
        let app = build_platform_router(test_state());
        let response = app
            .oneshot(
                Request::get("/observe/os-apps")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let apps = json["apps"].as_array().unwrap();
        assert!(!apps.is_empty());
        assert_eq!(apps[0]["name"], "project-management");
    }

    #[tokio::test]
    async fn test_install_os_app_nonexistent_returns_404() {
        let app = build_platform_router(test_state());
        let response = app
            .oneshot(
                Request::post("/api/os-apps/nonexistent/install")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"tenant":"test"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
