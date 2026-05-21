//! Integration tests for OData read handlers.
//!
//! Verifies entity set listing, single entity fetch, metadata,
//! service document, and error responses via the axum router.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{build_default_state, dispatch};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::build_router;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = common::CSDL_XML;
const ORDER_IOA: &str = common::ORDER_IOA;

/// Send a GET request to the router and return status + parsed JSON body.
async fn get_json(
    state: &temper_server::ServerState,
    path: &str,
) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let req = Request::builder().uri(path).body(Body::empty()).unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn post_json(
    state: &ServerState,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let req = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

async fn patch_json(
    state: &ServerState,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let req = Request::builder()
        .method(axum::http::Method::PATCH)
        .uri(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

fn build_turso_state(system_name: &str, store: TursoEventStore) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );

    let state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    {
        let mut registry = state.registry.write().unwrap();
        registry.set_verification_status(
            &TenantId::default(),
            "Order",
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![EntityLevelSummary {
                    level: "L0 SMT".to_string(),
                    passed: true,
                    summary: "OK".to_string(),
                    details: None,
                }],
                verified_at: "2026-04-15T00:00:00Z".to_string(),
            }),
        );
    }

    let mut state = state;
    state.set_storage_stack(StorageStack::from_turso(store));
    state
}

#[tokio::test]
async fn entity_set_returns_created_entities() {
    let (state, _sim) = build_default_state(42, "odata-read-set");
    let tenant = TenantId::default();

    dispatch(
        &state,
        &tenant,
        "Order",
        "ord-1",
        "Create",
        serde_json::json!({}),
    )
    .await
    .expect("create ord-1");
    dispatch(
        &state,
        &tenant,
        "Order",
        "ord-2",
        "Create",
        serde_json::json!({}),
    )
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

    dispatch(
        &state,
        &tenant,
        "Order",
        "ord-1",
        "Create",
        serde_json::json!({}),
    )
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
    assert!(
        !values.is_empty(),
        "service document should list entity sets"
    );
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

#[tokio::test]
async fn filtered_entity_set_returns_entities_created_via_odata_post() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-odata-read-create-filter-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let state = build_turso_state("odata-read-create-filter", store);

    let (status, body) = post_json(
        &state,
        "/tdata/Orders",
        serde_json::json!({
            "id": "ord-created-filter",
            "Currency": "USD",
            "Notes": "created through odata"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    let (status, body) = get_json(&state, "/tdata/Orders?$filter=Currency%20eq%20'USD'").await;
    assert_eq!(status, StatusCode::OK);
    let values = body["value"].as_array().expect("value array");
    assert_eq!(
        values.len(),
        1,
        "filtered reads should include entities created via OData POST: {body:?}"
    );
    assert_eq!(values[0]["entity_id"].as_str(), Some("ord-created-filter"));

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn filtered_entity_set_repairs_missing_catalog_rows_before_trusting_pushdown() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-odata-read-missing-catalog-filter-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let state = build_turso_state("odata-read-missing-catalog-filter", store.clone());

    let entity_id = "ord-missing-catalog-filter";
    let (status, body) = post_json(
        &state,
        "/tdata/Orders",
        serde_json::json!({
            "id": entity_id,
            "Currency": "USD",
            "Notes": "catalog row will be removed"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    store
        .remove_query_projection(TenantId::default().as_str(), "Order", entity_id)
        .await
        .expect("remove catalog row to simulate projection drift");

    let (status, body) = get_json(&state, "/tdata/Orders?$filter=Status%20eq%20'Draft'").await;
    assert_eq!(status, StatusCode::OK);
    let values = body["value"].as_array().expect("value array");
    assert!(
        values
            .iter()
            .any(|value| value["entity_id"].as_str() == Some(entity_id)),
        "filtered reads should hydrate missing catalog rows before trusting pushdown: {body:?}"
    );

    let repaired = store
        .query_field_index(
            TenantId::default().as_str(),
            "Order",
            "status = ?3",
            vec!["Draft".to_string()],
        )
        .await
        .expect("query repaired status projection");
    assert!(
        repaired.iter().any(|id| id == entity_id),
        "actor fallback should repair the durable catalog for future pushdown reads: {repaired:?}"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn filtered_entity_set_reflects_odata_patch_updates() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-odata-read-patch-filter-{}.db",
        uuid::Uuid::new_v4()
    ));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let state = build_turso_state("odata-read-patch-filter", store);

    let (status, body) = post_json(
        &state,
        "/tdata/Orders",
        serde_json::json!({
            "id": "ord-patched-filter",
            "Currency": "EUR",
            "Notes": "starts in eur"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    let (status, body) = patch_json(
        &state,
        "/tdata/Orders('ord-patched-filter')",
        serde_json::json!({
            "Currency": "USD"
        }),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "patch failed: {body:?}");

    let (status, body) = get_json(&state, "/tdata/Orders?$filter=Currency%20eq%20'USD'").await;
    assert_eq!(status, StatusCode::OK);
    let values = body["value"].as_array().expect("value array");
    assert!(
        values
            .iter()
            .any(|value| value["entity_id"].as_str() == Some("ord-patched-filter")),
        "filtered reads should reflect OData PATCH updates: {body:?}"
    );

    let _ = std::fs::remove_file(db_path);
}
