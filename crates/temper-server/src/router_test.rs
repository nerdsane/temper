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
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Order".to_string(), order_ioa.to_string());
    ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs)
}

fn test_state_with_order_and_payment_ioa() -> ServerState {
    let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    let order_ioa = include_str!("../../../test-fixtures/specs/order.ioa.toml");
    let csdl = parse_csdl(csdl_xml).unwrap();
    let system = ActorSystem::new("test-ioa-order-payment");
    let mut specs = std::collections::BTreeMap::new();
    specs.insert("Order".to_string(), order_ioa.to_string());
    // For navigation tests we only need entity creation/read, so reuse the same minimal IOA.
    specs.insert("Payment".to_string(), order_ioa.to_string());
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
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
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
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
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
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["@odata.context"], "$metadata#Orders");
}

#[tokio::test]
async fn test_entity_by_key_not_found() {
    let app = build_router(test_state());
    let response = app
        .oneshot(
            Request::get("/tdata/Orders('abc-123')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Nonexistent entity returns 404 (no transition table = no actor)
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_entity_by_key_found() {
    let app = build_router(test_state_with_ioa());

    // First create an entity via POST
    let create_response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "test-1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(create_response.status(), StatusCode::CREATED);

    // Now GET the created entity
    let response = app
        .oneshot(
            Request::get("/tdata/Orders('test-1')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
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
    let app = build_router(test_state_with_ioa());
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
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
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

#[tokio::test]
async fn test_post_body_used_for_entity_creation() {
    let app = build_router(test_state_with_ioa());

    // Create with specific ID and fields
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "order-42", "customer": "Bob"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    // Verify the body fields were stored
    assert_eq!(json["fields"]["customer"], "Bob");
    assert_eq!(json["fields"]["id"], "order-42");
}

#[tokio::test]
async fn test_entity_set_returns_created_entities() {
    let app = build_router(test_state_with_ioa());

    // Create two entities
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "o1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "o2", "customer": "Bob"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // GET the entity set — should return both entities
    let response = app
        .oneshot(Request::get("/tdata/Orders").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let values = json["value"].as_array().unwrap();
    assert_eq!(values.len(), 2);
}

#[tokio::test]
async fn test_patch_updates_entity() {
    let app = build_router(test_state_with_ioa());

    // Create entity
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "p1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // PATCH the entity
    let response = app
        .clone()
        .oneshot(
            Request::patch("/tdata/Orders('p1')")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"customer": "Bob"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["fields"]["customer"], "Bob");
}

#[tokio::test]
async fn test_delete_removes_entity() {
    let app = build_router(test_state_with_ioa());

    // Create entity
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "d1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // DELETE
    let response = app
        .clone()
        .oneshot(
            Request::delete("/tdata/Orders('d1')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    // GET should now return 404
    let response = app
        .oneshot(
            Request::get("/tdata/Orders('d1')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_patch_nonexistent_returns_404() {
    let app = build_router(test_state_with_ioa());
    let response = app
        .oneshot(
            Request::patch("/tdata/Orders('nope')")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"customer": "Bob"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_delete_nonexistent_returns_404() {
    let app = build_router(test_state_with_ioa());
    let response = app
        .oneshot(
            Request::delete("/tdata/Orders('nope')")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_navigation_property_single_entity() {
    let app = build_router(test_state_with_order_and_payment_ioa());

    // Create parent order.
    let order_create = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "ord-nav-1", "customer": "Alice"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(order_create.status(), StatusCode::CREATED);

    // Create related payment linked by OrderId.
    let payment_create = app
        .clone()
        .oneshot(
            Request::post("/tdata/Payments")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "pay-nav-1", "OrderId": "ord-nav-1"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(payment_create.status(), StatusCode::CREATED);

    let response = app
        .oneshot(
            Request::get("/tdata/Orders('ord-nav-1')/Payment")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["entity_type"], "Payment");
    assert_eq!(json["fields"]["OrderId"], "ord-nav-1");
}

#[tokio::test]
async fn test_navigation_property_not_found_returns_404() {
    let app = build_router(test_state_with_ioa());
    let _ = app
        .clone()
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(r#"{"id": "ord-nav-missing"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    let response = app
        .oneshot(
            Request::get("/tdata/Orders('ord-nav-missing')/DefinitelyMissingNav")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_temper_client_script_served() {
    let app = build_router(test_state());
    let response = app
        .oneshot(
            Request::get("/temper-client.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("Content-Type").unwrap(),
        "application/javascript"
    );
    assert_eq!(
        response.headers().get("Cache-Control").unwrap(),
        "public, max-age=3600"
    );
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    assert!(body_str.contains("Temper"));
}

#[tokio::test]
async fn test_temper_client_script_alias_served() {
    let app = build_router(test_state());
    let response = app
        .oneshot(
            Request::get("/static/temper-client.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get("Content-Type").unwrap(),
        "application/javascript"
    );
}

#[tokio::test]
async fn test_cors_header_present() {
    let app = build_router(test_state());
    let response = app
        .oneshot(
            Request::get("/tdata/Orders")
                .header("Origin", "http://localhost:5173")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response
            .headers()
            .get("Access-Control-Allow-Origin")
            .unwrap(),
        "*"
    );
}
