//! Axum router construction for the Temper Data API.

use axum::routing::get;
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::dispatch;
use crate::state::ServerState;

/// Build the axum router with all Temper Data API routes.
///
/// Route structure:
/// - GET  /tdata                      → service document
/// - GET  /tdata/$metadata            → CSDL XML (tenant-scoped)
/// - GET  /tdata/$hints               → agent hints JSON
/// - GET  /tdata/{*path}              → entity set / entity / navigation / function
/// - POST /tdata/{*path}              → create entity / bound action
///
/// Tenant is extracted from the `X-Tenant-Id` header. Falls back to the
/// first registered tenant in the SpecRegistry.
pub fn build_router(state: ServerState) -> Router {
    let tdata = Router::new()
        .route("/", get(dispatch::handle_service_document))
        .route("/$metadata", get(dispatch::handle_metadata))
        .route("/$hints", get(dispatch::handle_hints))
        .route("/{*path}", get(dispatch::handle_odata_get).post(dispatch::handle_odata_post));

    Router::new()
        .nest("/tdata", tdata)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use temper_runtime::ActorSystem;
    use temper_spec::csdl::parse_csdl;
    use tower::ServiceExt;

    fn test_state() -> ServerState {
        let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
        let csdl = parse_csdl(csdl_xml).unwrap();
        let system = ActorSystem::new("test");
        ServerState::new(system, csdl, csdl_xml.to_string())
    }

    fn test_state_with_ioa() -> ServerState {
        let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
        let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml");
        let csdl = parse_csdl(csdl_xml).unwrap();
        let system = ActorSystem::new("test-ioa");
        let mut specs = std::collections::HashMap::new();
        specs.insert("Order".to_string(), order_ioa.to_string());
        ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs)
    }

    #[tokio::test]
    async fn test_service_document() {
        let app = build_router(test_state());
        let response = app
            .oneshot(Request::get("/tdata").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["value"].is_array());
        assert_eq!(json["@odata.context"], "$metadata");
    }

    #[tokio::test]
    async fn test_metadata_endpoint() {
        let app = build_router(test_state());
        let response = app
            .oneshot(
                Request::get("/tdata/$metadata")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let content_type = response.headers().get("Content-Type").unwrap();
        assert_eq!(content_type, "application/xml");
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let body_str = std::str::from_utf8(&body).unwrap();
        assert!(body_str.contains("edmx:Edmx"));
        assert!(body_str.contains("Temper.Example"));
    }

    #[tokio::test]
    async fn test_entity_set_listing() {
        let app = build_router(test_state());
        let response = app
            .oneshot(Request::get("/tdata/Orders").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["@odata.context"], "$metadata#Orders");
    }

    #[tokio::test]
    async fn test_entity_by_key() {
        let app = build_router(test_state());
        let response = app
            .oneshot(
                Request::get("/tdata/Orders('abc-123')")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["@odata.context"], "$metadata#Orders/$entity");
    }

    #[tokio::test]
    async fn test_unknown_entity_set_returns_404() {
        let app = build_router(test_state());
        let response = app
            .oneshot(
                Request::get("/tdata/NonExistent")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_post_entity_creation() {
        let app = build_router(test_state());
        let response = app
            .oneshot(
                Request::post("/tdata/Orders")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"status": "Draft"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::CREATED);
    }

    #[tokio::test]
    async fn test_post_bound_action() {
        let app = build_router(test_state_with_ioa());
        let response = app
            .oneshot(
                Request::post("/tdata/Orders('abc-123')/Temper.Example.CancelOrder")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"Reason": "changed mind"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "Cancelled");
    }

    #[tokio::test]
    async fn test_odata_version_header() {
        let app = build_router(test_state());
        let response = app
            .oneshot(Request::get("/tdata/Orders").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let odata_version = response.headers().get("OData-Version").unwrap();
        assert_eq!(odata_version, "4.0");
    }

    #[tokio::test]
    async fn test_old_odata_path_returns_404() {
        let app = build_router(test_state());
        let response = app
            .oneshot(Request::get("/odata").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
}
