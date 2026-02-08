//! Platform router construction.
//!
//! Assembles the full axum router with developer UI, production UI,
//! WebSocket endpoints, and nested OData routes.

use axum::response::Html;
use axum::routing::get;
use axum::Router;

use crate::state::PlatformState;
use crate::ws;

/// Developer UI HTML (inlined at compile time).
const DEV_HTML: &str = include_str!("static_files/dev.html");

/// Production UI HTML (inlined at compile time).
const PROD_HTML: &str = include_str!("static_files/prod.html");

/// Build the full platform router.
///
/// Route structure:
/// - `GET  /dev`       → Developer chat UI (split-pane)
/// - `WS   /dev/ws`    → Developer WebSocket
/// - `GET  /prod`      → Production chat UI
/// - `WS   /prod/ws`   → Production WebSocket
/// - `GET  /odata/...` → OData entity API (from temper-server)
/// - `POST /odata/...` → OData actions (from temper-server)
pub fn build_platform_router(state: PlatformState) -> Router {
    // OData router has ServerState — finalize it to Router<()> before merging.
    let odata: Router = temper_server::build_router(state.server.clone());

    let platform = Router::new()
        .route("/dev", get(serve_dev_html))
        .route("/dev/ws", get(ws::ws_dev_handler))
        .route("/prod", get(serve_prod_html))
        .route("/prod/ws", get(ws::ws_prod_handler))
        .with_state(state);

    platform.merge(odata)
}

/// Serve the developer UI HTML.
async fn serve_dev_html() -> Html<&'static str> {
    Html(DEV_HTML)
}

/// Serve the production UI HTML.
async fn serve_prod_html() -> Html<&'static str> {
    Html(PROD_HTML)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    fn test_state() -> PlatformState {
        PlatformState::new_dev(None)
    }

    #[tokio::test]
    async fn test_dev_html_served() {
        let app = build_platform_router(test_state());
        let response = app
            .oneshot(Request::get("/dev").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("Temper Developer"));
    }

    #[tokio::test]
    async fn test_prod_html_served() {
        let app = build_platform_router(test_state());
        let response = app
            .oneshot(Request::get("/prod").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let html = std::str::from_utf8(&body).unwrap();
        assert!(html.contains("<!DOCTYPE html>"));
        assert!(html.contains("Temper Production"));
    }

    #[tokio::test]
    async fn test_odata_routes_accessible() {
        let app = build_platform_router(test_state());
        let response = app
            .oneshot(Request::get("/odata").body(Body::empty()).unwrap())
            .await
            .unwrap();

        // OData service document should be accessible
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
}
