//! Platform router construction.
//!
//! Assembles the full axum router with tenant-aware routing.
//! The API is the **Temper Data API** at `/tdata`.

use axum::Router;
use axum::middleware;

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

    temper_server::build_router(state.server.clone())
        .nest("/api", tenant_api.with_state(state.clone()))
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
            .oneshot(Request::get("/tdata").body(Body::empty()).unwrap())
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
}
