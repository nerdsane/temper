//! Integration tests for `[[field_invariant]]` enforcement on the OData write
//! path.
//!
//! Exercises the full pipeline: `build_router` → `POST /tdata/...` →
//! [`pre_upsert_field_invariant_checks`](temper_server::odata::constraints::pre_upsert_field_invariant_checks)
//! → 409 `field_invariant` response. Uses a synthetic two-state entity
//! (`Container`) built around the existing `Order` CSDL — `Order` has scalar
//! string properties (`Currency`, `Notes`) that the invariants can key off
//! without requiring a bespoke CSDL fixture.
//!
//! Invariant under test: **when `Currency == "USD"`, `Notes` must be
//! absent**. This captures the full combinator surface: leaf `equals` on the
//! `when` side, leaf `absent = true` on the `require` side, optional
//! configured `message`.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::CSDL_XML;
use temper_runtime::ActorSystem;
use temper_runtime::persistence::schema_deployment::{SchemaScope, SchemaScopeKind};
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

/// Order IOA spec extended with a `[[field_invariant]]` rule. `Order` is in
/// the fixture CSDL, so we just reuse its state machine and layer the new
/// rule on top.
const ORDER_WITH_FIELD_INVARIANT_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
kind = "internal"
from = ["Draft"]
to = "Submitted"

[[field_invariant]]
name = "UsdOrdersCannotHaveNotes"
when = { field = "Currency", equals = "USD" }
require = { field = "Notes", absent = true }
message = "USD orders cannot carry notes"
"#;

fn authenticate(mut request: Request<Body>) -> Request<Body> {
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "field-invariant-test",
                "test-agent",
                None,
            ),
        ));
    request
}

fn build_state_with_field_invariant() -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_WITH_FIELD_INVARIANT_IOA)],
    );
    let system = ActorSystem::new("field-invariant-test");
    let state = ServerState::from_registry(system, registry);
    // Mark Order verified so write requests aren't rejected by the
    // verification gate — these tests exercise the constraint pipeline, not
    // the verification cascade.
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
                verified_at: "2026-04-11T00:00:00Z".to_string(),
            }),
        );
    }
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("field-invariant fixture policy should parse");
    state
}

/// Send a POST `/tdata/Orders` with the given JSON body and return the
/// status + parsed body.
async fn post_order(state: &ServerState, body: &str) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let req = authenticate(
        Request::post("/tdata/Orders")
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    );
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

/// Send a PATCH `/tdata/Orders('<id>')` with the given JSON body.
async fn patch_order(state: &ServerState, id: &str, body: &str) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let req = authenticate(
        Request::builder()
            .method(axum::http::Method::PATCH)
            .uri(format!("/tdata/Orders('{id}')"))
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    );
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

#[tokio::test]
async fn post_usd_without_notes_is_allowed() {
    let state = build_state_with_field_invariant();
    let (status, body) = post_order(&state, r#"{"id": "ord-usd-ok", "Currency": "USD"}"#).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "when `when` matches and `require` holds, write should succeed: {body:?}"
    );
}

#[tokio::test]
async fn post_usd_with_notes_is_rejected() {
    let state = build_state_with_field_invariant();
    let (status, body) = post_order(
        &state,
        r#"{"id": "ord-usd-fail", "Currency": "USD", "Notes": "rush"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "USD + Notes should violate the field invariant: {body:?}"
    );
    assert_eq!(body["error"]["code"].as_str(), Some("ConstraintViolation"));
    assert_eq!(
        body["error"]["details"]["type"].as_str(),
        Some("field_invariant"),
        "violation type should be `field_invariant`"
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("UsdOrdersCannotHaveNotes")
    );
    assert_eq!(
        body["error"]["message"].as_str(),
        Some("USD orders cannot carry notes"),
        "error message should match the spec-configured message"
    );
}

#[tokio::test]
async fn post_eur_with_notes_is_allowed() {
    // Non-matching `when` → rule is inert, Notes is unrestricted.
    let state = build_state_with_field_invariant();
    let (status, body) = post_order(
        &state,
        r#"{"id": "ord-eur-ok", "Currency": "EUR", "Notes": "eu order"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "EUR order should be allowed to have Notes: {body:?}"
    );
}

#[tokio::test]
async fn patch_that_triggers_violation_is_rejected() {
    // Start with an EUR order that has Notes (fine — `when` doesn't match).
    let state = build_state_with_field_invariant();
    let (status, body) = post_order(
        &state,
        r#"{"id": "ord-patch", "Currency": "EUR", "Notes": "keep me"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    // PATCH it to Currency=USD while Notes still set → violation.
    let (status, body) = patch_order(&state, "ord-patch", r#"{"Currency": "USD"}"#).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "PATCH that flips Currency into a matching `when` should be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["type"].as_str(),
        Some("field_invariant")
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("UsdOrdersCannotHaveNotes")
    );
}

#[tokio::test]
async fn patch_that_satisfies_rule_is_allowed() {
    // Start with EUR + Notes (fine).
    let state = build_state_with_field_invariant();
    let (status, body) = post_order(
        &state,
        r#"{"id": "ord-patch-ok", "Currency": "EUR", "Notes": "drop me"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed create failed: {body:?}");

    // PATCH to USD and simultaneously clear Notes → should be allowed.
    let (status, body) = patch_order(
        &state,
        "ord-patch-ok",
        r#"{"Currency": "USD", "Notes": null}"#,
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "PATCH that clears Notes before flipping to USD should succeed; got {status} body={body:?}"
    );
}

#[tokio::test]
async fn field_invariant_bypass_respects_feature_flag() {
    // When `cross_invariant_enforce` is false, the check is bypassed.
    // This verifies the shared feature flag convention.
    let mut state = build_state_with_field_invariant();
    state.cross_invariant_enforce = false;
    let (status, body) = post_order(
        &state,
        r#"{"id": "ord-bypass", "Currency": "USD", "Notes": "bypassed"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "with enforcement disabled, invariant should be bypassed: {body:?}"
    );
}

#[tokio::test]
async fn scoped_create_enforces_the_exact_bundle_field_invariant() {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL parse");
    let scope = SchemaScope {
        kind: SchemaScopeKind::Task,
        id: "field-invariant-task".into(),
    };
    let digest = format!("sha256:{}", "d".repeat(64));
    let mut registry = SpecRegistry::new();
    registry
        .stage_scoped_bundle(
            TenantId::default(),
            scope.clone(),
            digest.clone(),
            csdl,
            CSDL_XML.to_string(),
            &[("Order", ORDER_WITH_FIELD_INVARIANT_IOA)],
        )
        .expect("scoped bundle should stage");
    registry
        .activate_scoped_bundle(&TenantId::default(), &scope, &digest, None)
        .expect("scoped bundle should activate");
    let state = ServerState::from_registry(ActorSystem::new("scoped-field-invariant"), registry);
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("scoped field-invariant fixture policy should parse");
    let response = build_router(state)
        .oneshot(authenticate(
            Request::post("/tdata/Orders")
                .header("content-type", "application/json")
                .header("x-temper-schema-scope-kind", "task")
                .header("x-temper-schema-scope-id", "field-invariant-task")
                .body(Body::from(
                    r#"{"id":"scoped-invalid","Currency":"USD","Notes":"forbidden"}"#,
                ))
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CONFLICT);
}
