//! Integration tests for OData read handlers.
//!
//! Verifies entity set listing, single entity fetch, metadata,
//! service document, and error responses via the axum router.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{build_default_state, dispatch};
use temper_runtime::tenant::TenantId;
use temper_server::build_router;
use tower::ServiceExt;

/// Send a GET request to the router and return status + parsed JSON body.
async fn get_json(
    state: &temper_server::ServerState,
    path: &str,
) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let req = Request::builder()
        .uri(path)
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

#[tokio::test]
async fn entity_set_returns_created_entities() {
    let (state, _sim) = build_default_state(42, "odata-read-set");
    let tenant = TenantId::default();

    dispatch(&state, &tenant, "Order", "ord-1", "Create", serde_json::json!({}))
        .await
        .expect("create ord-1");
    dispatch(&state, &tenant, "Order", "ord-2", "Create", serde_json::json!({}))
        .await
        .expect("create ord-2");

    let (status, body) = get_json(&state, "/tdata/Orders").await;
    assert_eq!(status, StatusCode::OK);
    let values = body["value"].as_array().expect("value array");
    assert_eq!(values.len(), 2);
    assert!(body["@odata.context"].as_str().unwrap().contains("Orders"));
}

#[tokio::test]
async fn entity_get_returns_single_entity_with_actions() {
    let (state, _sim) = build_default_state(43, "odata-read-entity");
    let tenant = TenantId::default();

    dispatch(&state, &tenant, "Order", "ord-1", "Create", serde_json::json!({}))
        .await
        .expect("create");

    let (status, body) = get_json(&state, "/tdata/Orders('ord-1')").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["entity_id"].as_str(), Some("ord-1"));
    // Should have @odata.actions enrichment
    assert!(body["@odata.actions"].is_array());
    // Should have @odata.children enrichment
    assert!(body["@odata.children"].is_object());
}

#[tokio::test]
async fn entity_not_found_returns_404() {
    let (state, _sim) = build_default_state(44, "odata-read-404");

    let (status, body) = get_json(&state, "/tdata/Orders('nonexistent')").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_object());
}

#[tokio::test]
async fn entity_set_not_found_returns_404() {
    let (state, _sim) = build_default_state(45, "odata-read-noset");

    let (status, body) = get_json(&state, "/tdata/NonexistentSet").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].is_object());
}

#[tokio::test]
async fn service_document_lists_entity_sets() {
    let (state, _sim) = build_default_state(47, "odata-read-svc");

    let (status, body) = get_json(&state, "/tdata").await;
    assert_eq!(status, StatusCode::OK);
    let values = body["value"].as_array().expect("value array");
    assert!(!values.is_empty(), "service document should list entity sets");
}

#[tokio::test]
async fn metadata_returns_csdl_xml() {
    let (state, _sim) = build_default_state(46, "odata-read-meta");
    let router = build_router(state);
    let req = Request::builder()
        .uri("/tdata/$metadata")
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let body_str = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(body_str.contains("edmx:Edmx"), "should return CSDL XML");
}
