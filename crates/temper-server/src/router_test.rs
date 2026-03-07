use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

use crate::events::EntityStateChange;

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
    ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs).unwrap()
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
    ServerState::with_specs(system, csdl, csdl_xml.to_string(), specs).unwrap()
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
                .header("X-Temper-Principal-Kind", "admin")
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

/// Read SSE data from an axum body stream until `predicate` matches or timeout expires.
async fn collect_sse_frames_until(
    body: Body,
    predicate: impl Fn(&str) -> bool,
    timeout_ms: u64,
) -> String {
    use tokio_stream::StreamExt as _;

    let mut stream = body.into_data_stream();
    let mut collected = String::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(std::time::Duration::from_millis(500), stream.next()).await {
            Ok(Some(Ok(bytes))) => {
                collected.push_str(&String::from_utf8_lossy(&bytes));
                if predicate(&collected) {
                    break;
                }
            }
            Ok(Some(Err(_))) | Ok(None) => break,
            Err(_) => continue, // timeout on this chunk, try again
        }
    }
    collected
}

#[tokio::test]
async fn test_sse_events_endpoint_delivers_state_changes() {
    let state = test_state_with_ioa();
    let event_tx = state.event_tx.clone();
    let app = build_router(state);

    // Connect to SSE endpoint — response should be 200 with text/event-stream.
    let response = app
        .oneshot(
            Request::get("/tdata/$events")
                .header("X-Temper-Principal-Kind", "admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        response
            .headers()
            .get("content-type")
            .unwrap()
            .to_str()
            .unwrap()
            .contains("text/event-stream"),
    );

    // Send a state change event on the broadcast channel.
    let _ = event_tx.send(EntityStateChange {
        entity_type: "Order".into(),
        entity_id: "o-sse-1".into(),
        action: "SubmitOrder".into(),
        status: "Submitted".into(),
        tenant: "default".into(),
        agent_id: Some("test-agent".into()),
        session_id: None,
    });

    // Read SSE frames until we see the event (stream never closes on its own).
    let collected =
        collect_sse_frames_until(response.into_body(), |s| s.contains("o-sse-1"), 3000).await;
    assert!(
        collected.contains("o-sse-1"),
        "SSE body should contain the entity_id. Got: {collected}"
    );
    assert!(
        collected.contains("SubmitOrder"),
        "SSE body should contain the action. Got: {collected}"
    );
}

#[tokio::test]
async fn test_sse_events_lagged_receiver_continues() {
    let state = test_state_with_ioa();
    let event_tx = state.event_tx.clone();

    // The broadcast channel capacity is 256 (set in ServerState constructors).
    // Flood it before any subscriber — then subscribe and send one more event.
    for i in 0..300 {
        let _ = event_tx.send(EntityStateChange {
            entity_type: "Order".into(),
            entity_id: format!("flood-{i}"),
            action: "Flood".into(),
            status: "Flooded".into(),
            tenant: "default".into(),
            agent_id: None,
            session_id: None,
        });
    }

    let app = build_router(state);
    let response = app
        .oneshot(
            Request::get("/tdata/$events")
                .header("X-Temper-Principal-Kind", "admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Send a fresh event that should be delivered.
    let _ = event_tx.send(EntityStateChange {
        entity_type: "Order".into(),
        entity_id: "after-flood".into(),
        action: "Fresh".into(),
        status: "OK".into(),
        tenant: "default".into(),
        agent_id: None,
        session_id: None,
    });

    // Read frames — the stream should recover and deliver the fresh event.
    let collected =
        collect_sse_frames_until(response.into_body(), |s| s.contains("after-flood"), 3000).await;
    assert!(
        collected.contains("after-flood"),
        "SSE should recover after lag. Got: {collected}"
    );
}
