//! Axum router construction for OData endpoints.

use axum::routing::get;
use axum::Router;
use tower_http::trace::TraceLayer;

use crate::dispatch;
use crate::state::ServerState;


/// Build the axum router with all OData routes.
///
/// Route structure:
/// - GET  /odata                      → service document
/// - GET  /odata/$metadata            → CSDL XML
/// - GET  /odata/{*path}              → entity set / entity / navigation / function
/// - POST /odata/{*path}              → create entity / bound action
pub fn build_router(state: ServerState) -> Router {
    let odata = Router::new()
        .route("/", get(dispatch::handle_service_document))
        .route("/$metadata", get(dispatch::handle_metadata))
        .route("/{*path}", get(dispatch::handle_odata_get).post(dispatch::handle_odata_post));

    Router::new()
        .nest("/odata", odata)
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

    fn test_state_with_tla() -> ServerState {
        let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
        let order_tla = include_str!("../../../test-fixtures/specs/order.tla");
        let csdl = parse_csdl(csdl_xml).unwrap();
        let system = ActorSystem::new("test-tla");
        let mut tla = std::collections::HashMap::new();
        tla.insert("Order".to_string(), order_tla.to_string());
        ServerState::with_tla(system, csdl, csdl_xml.to_string(), tla)
    }

    #[tokio::test]
    async fn test_service_document() {
        let app = build_router(test_state());
        let response = app
            .oneshot(Request::get("/odata").body(Body::empty()).unwrap())
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
                Request::get("/odata/$metadata")
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
        assert!(body_str.contains("Temper.Ecommerce"));
    }

    #[tokio::test]
    async fn test_entity_set_listing() {
        let app = build_router(test_state());
        let response = app
            .oneshot(Request::get("/odata/Orders").body(Body::empty()).unwrap())
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
                Request::get("/odata/Orders('abc-123')")
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
                Request::get("/odata/NonExistent")
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
                Request::post("/odata/Orders")
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
        let app = build_router(test_state_with_tla());
        let response = app
            .oneshot(
                Request::post("/odata/Orders('abc-123')/Temper.Ecommerce.CancelOrder")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"Reason": "changed mind"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

        // Real dispatch: CancelOrder from Draft → Cancelled (200 OK)
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["status"], "Cancelled");
    }

    #[tokio::test]
    async fn test_odata_version_header() {
        let app = build_router(test_state());
        let response = app
            .oneshot(Request::get("/odata/Orders").body(Body::empty()).unwrap())
            .await
            .unwrap();

        let odata_version = response.headers().get("OData-Version").unwrap();
        assert_eq!(odata_version, "4.0");
    }
}
