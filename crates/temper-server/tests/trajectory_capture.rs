//! End-to-end trajectory capture over the HTTP surface.
//!
//! Covers the two properties a JCS trajectory consumer depends on:
//!
//! 1. A **successful** governed action produces a durable trajectory row that
//!    carries its `request_body` — not only failures, which is all the capture
//!    path used to record.
//! 2. `X-Session-Id` and `X-Intent` travel from the HTTP request all the way
//!    into the persisted row, on both the success and the failure path.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::build_router;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::state::TrajectoryEntry;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = common::CSDL_XML;
const ORDER_IOA: &str = common::ORDER_IOA;

const SESSION_ID: &str = "sess-jcs-e2e";
const INTENT: &str = "add a line item to the draft order";

/// Build a Turso-backed state so trajectory rows land in a real sink.
///
/// The sim store has no trajectory capability, so a durable backend is the
/// only way to assert on what was actually persisted.
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
                verified_at: "2026-08-11T00:00:00Z".to_string(),
            }),
        );
    }

    let mut state = state;
    state.set_storage_stack(StorageStack::from_turso(store));
    state
}

async fn temp_store(label: &str) -> (TursoEventStore, std::path::PathBuf) {
    let db_path = std::env::temp_dir().join(format!("temper-{label}-{}.db", uuid::Uuid::new_v4()));
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    (store, db_path)
}

/// POST with the observability headers under test.
async fn post_observed(
    state: &ServerState,
    path: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let req = Request::post(path)
        .header("Content-Type", "application/json")
        .header("X-Session-Id", SESSION_ID)
        .header("X-Intent", INTENT)
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let parsed = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, parsed)
}

/// Wait for a persisted trajectory row matching `predicate`.
///
/// Trajectory persistence is a background outbox (ADR-0067), so the row lands
/// after the HTTP response returns.
async fn await_trajectory(
    state: &ServerState,
    label: &str,
    predicate: impl Fn(&TrajectoryEntry) -> bool,
) -> TrajectoryEntry {
    for _ in 0..200 {
        if let Some(found) = state
            .load_trajectory_entries(200)
            .await
            .into_iter()
            .find(&predicate)
        {
            return found;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let seen: Vec<String> = state
        .load_trajectory_entries(200)
        .await
        .into_iter()
        .map(|e| format!("{}.{} success={}", e.entity_type, e.action, e.success))
        .collect();
    panic!("no trajectory row matched '{label}'; rows seen: {seen:?}");
}

#[tokio::test]
async fn successful_governed_action_persists_request_body_session_and_intent() {
    let (store, db_path) = temp_store("trajectory-success").await;
    let state = build_turso_state("trajectory-capture-success", store);

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders",
        serde_json::json!({"id": "ord-jcs-1", "Currency": "USD"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders('ord-jcs-1')/Temper.AddItem",
        serde_json::json!({"ProductId": "prod-9", "Quantity": 3}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "AddItem failed: {body:?}");

    let entry = await_trajectory(&state, "successful AddItem", |entry| {
        entry.action == "AddItem" && entry.entity_id == "ord-jcs-1" && entry.success
    })
    .await;

    assert!(entry.success, "the captured row is the successful action");
    assert_eq!(
        entry.session_id.as_deref(),
        Some(SESSION_ID),
        "X-Session-Id must reach the persisted trajectory row"
    );
    assert_eq!(
        entry.intent.as_deref(),
        Some(INTENT),
        "X-Intent must reach the persisted trajectory row"
    );

    let request_body = entry
        .request_body
        .as_ref()
        .expect("successful actions must persist their request body");
    assert_eq!(request_body["ProductId"], serde_json::json!("prod-9"));
    assert_eq!(request_body["Quantity"], serde_json::json!(3));

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn failed_governed_action_still_persists_request_body_session_and_intent() {
    let (store, db_path) = temp_store("trajectory-failure").await;
    let state = build_turso_state("trajectory-capture-failure", store);

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders",
        serde_json::json!({"id": "ord-jcs-2", "Currency": "USD"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    // SubmitOrder guards on `items > 0`; the fresh order has none, so the
    // guard rejects and the dispatch records a failed intent.
    let (status, _body) = post_observed(
        &state,
        "/tdata/Orders('ord-jcs-2')/Temper.SubmitOrder",
        serde_json::json!({"ShippingAddressId": "addr-1", "PaymentMethod": "card"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "SubmitOrder without items must fail the guard"
    );

    let entry = await_trajectory(&state, "failed SubmitOrder", |entry| {
        entry.action == "SubmitOrder" && entry.entity_id == "ord-jcs-2" && !entry.success
    })
    .await;

    assert_eq!(entry.session_id.as_deref(), Some(SESSION_ID));
    assert_eq!(entry.intent.as_deref(), Some(INTENT));
    let request_body = entry
        .request_body
        .as_ref()
        .expect("failed actions keep persisting their request body");
    assert_eq!(
        request_body["ShippingAddressId"],
        serde_json::json!("addr-1")
    );
    assert!(entry.error.is_some(), "the failure reason is recorded");

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn observe_prefixed_headers_are_honoured_as_session_and_intent() {
    // The canonical spellings are the `X-Temper-Observe-*` headers; the short
    // `X-Session-Id`/`X-Intent` forms are aliases. Both must land identically.
    let (store, db_path) = temp_store("trajectory-observe-headers").await;
    let state = build_turso_state("trajectory-capture-observe-headers", store);

    let router = build_router(state.clone());
    let resp = router
        .oneshot(
            Request::post("/tdata/Orders")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::json!({"id": "ord-jcs-3", "Currency": "USD"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let router = build_router(state.clone());
    let resp = router
        .oneshot(
            Request::post("/tdata/Orders('ord-jcs-3')/Temper.AddItem")
                .header("Content-Type", "application/json")
                .header("X-Temper-Observe-Session-Id", "sess-observe-prefixed")
                .header("X-Temper-Observe-Intent", "observe-prefixed intent")
                .body(Body::from(
                    serde_json::json!({"ProductId": "prod-3", "Quantity": 1}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let entry = await_trajectory(&state, "observe-prefixed AddItem", |entry| {
        entry.action == "AddItem" && entry.entity_id == "ord-jcs-3" && entry.success
    })
    .await;

    assert_eq!(
        entry.session_id.as_deref(),
        Some("sess-observe-prefixed"),
        "X-Temper-Observe-Session-Id must reach the persisted row"
    );
    assert_eq!(
        entry.intent.as_deref(),
        Some("observe-prefixed intent"),
        "X-Temper-Observe-Intent must reach the persisted row"
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn cedar_denied_action_persists_intent_and_evaluated_attributes() {
    // An authorization denial is the row the Evolution Engine reasons over.
    // Without the caller's intent and the attributes Cedar actually saw, the
    // denial says what was blocked but not what the agent was attempting.
    let (store, db_path) = temp_store("trajectory-denial").await;
    let state = build_turso_state("trajectory-capture-denial", store);

    let (status, body) = post_observed(
        &state,
        "/tdata/Orders",
        serde_json::json!({"id": "ord-jcs-4", "Currency": "USD"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    state
        .authz
        .reload_tenant_policies(
            TenantId::default().as_str(),
            r#"permit(principal, action in [Action::"list", Action::"read"], resource is Order);"#,
        )
        .expect("install Cedar policy");

    let (status, _body) = post_observed(
        &state,
        "/tdata/Orders('ord-jcs-4')/Temper.AddItem",
        serde_json::json!({"ProductId": "prod-denied", "Quantity": 1}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "AddItem must be denied under a read-only policy set"
    );

    let entry = await_trajectory(&state, "denied AddItem", |entry| {
        entry.action == "AddItem"
            && entry.entity_id == "ord-jcs-4"
            && entry.authz_denied == Some(true)
    })
    .await;

    assert_eq!(entry.intent.as_deref(), Some(INTENT));
    let request_body = entry
        .request_body
        .as_ref()
        .expect("denials must persist the attributes Cedar evaluated");
    assert_eq!(
        request_body["id"],
        serde_json::json!("ord-jcs-4"),
        "the evaluated resource attributes are recorded"
    );

    let _ = std::fs::remove_file(db_path);
}
