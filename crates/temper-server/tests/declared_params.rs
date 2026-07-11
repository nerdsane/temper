//! ARN-247 integration test: the OData bound-action boundary rejects
//! request-body params the action does not declare.
//!
//! Exercises the full pipeline: `build_router` → `POST /tdata/Orders('id')/Annotate`
//! → [`dispatch_bound_action`](temper_server::odata) declared-param check → 400
//! `UndeclaredActionParams`. Reuses the fixture `Order` CSDL (which has scalar
//! string properties `Currency` and `Notes`) and layers an IOA whose `Annotate`
//! action declares only `Notes`, so a body that also carries `Currency` is
//! undeclared for that action.
//!
//! This is the loud external half of the fix; the silent drop that covers every
//! internal dispatch path (spawn, composite, replay) is unit-tested in
//! `entity_actor::effects`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::CSDL_XML;
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

/// `Order` IOA with an `Annotate` action that declares only `Notes`. `Order` is
/// in the fixture CSDL, so we reuse its state machine and layer the action on top.
const ORDER_WITH_ANNOTATE_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[state]]
name = "Notes"
type = "string"
initial = ""

[[action]]
name = "Annotate"
kind = "input"
from = ["Draft"]
to = "Draft"
params = ["Notes"]
"#;

fn build_state() -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_WITH_ANNOTATE_IOA)],
    );
    let system = ActorSystem::new("declared-params-test");
    let state = ServerState::from_registry(system, registry);
    // Mark Order verified so writes aren't rejected by the verification gate —
    // this test exercises the declared-param boundary, not the cascade.
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
                verified_at: "2026-07-11T00:00:00Z".to_string(),
            }),
        );
    }
    state
}

async fn post(state: &ServerState, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let req = Request::post(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// GET the order and return its `fields` object (the projected entity state).
async fn get_order_fields(state: &ServerState, id: &str) -> serde_json::Value {
    let router = build_router(state.clone());
    let req = Request::get(format!("/tdata/Orders('{id}')"))
        .body(Body::empty())
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let entity: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    entity
        .get("fields")
        .cloned()
        .unwrap_or(serde_json::Value::Null)
}

#[tokio::test]
async fn bound_action_rejects_undeclared_param_and_leaves_fields_unchanged() {
    let state = build_state();

    // Seed an order with Currency = EUR.
    let (status, body) = post(
        &state,
        "/tdata/Orders",
        r#"{"id": "ord-1", "Currency": "EUR"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    // Invoke Annotate (declares only `Notes`) with an extra undeclared `Currency`.
    // OData bound actions are namespace-qualified; the kernel resolves the short
    // action name from the last dotted segment.
    let (status, body) = post(
        &state,
        "/tdata/Orders('ord-1')/Temper.Annotate",
        r#"{"Notes": "legit", "Currency": "USD"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "undeclared `Currency` must be rejected at the boundary: {body:?}"
    );
    assert_eq!(
        body["error"]["code"].as_str(),
        Some("UndeclaredActionParams"),
        "error code should name the undeclared-param rejection: {body:?}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("Currency"),
        "error message should name the offending param: {body:?}"
    );

    // The rejected action must not have mutated anything: Currency still EUR,
    // Notes never written.
    let fields = get_order_fields(&state, "ord-1").await;
    assert_eq!(
        fields["Currency"].as_str(),
        Some("EUR"),
        "rejected action must not overwrite Currency: {fields:?}"
    );
    assert_ne!(
        fields["Notes"].as_str(),
        Some("legit"),
        "rejected action must not write its declared param either: {fields:?}"
    );
}

#[tokio::test]
async fn bound_action_with_only_declared_params_succeeds() {
    let state = build_state();

    let (status, body) = post(
        &state,
        "/tdata/Orders",
        r#"{"id": "ord-2", "Currency": "EUR"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    let (status, body) = post(
        &state,
        "/tdata/Orders('ord-2')/Temper.Annotate",
        r#"{"Notes": "hello"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::OK,
        "declared-only Annotate should succeed: {body:?}"
    );

    let fields = get_order_fields(&state, "ord-2").await;
    assert_eq!(
        fields["Notes"].as_str(),
        Some("hello"),
        "declared param must be written: {fields:?}"
    );
    assert_eq!(
        fields["Currency"].as_str(),
        Some("EUR"),
        "pre-existing field preserved: {fields:?}"
    );
}
