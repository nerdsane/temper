#![cfg(feature = "observe")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::{ServerState, SpecRegistry, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn test_state_with_registry() -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    ServerState::from_registry(ActorSystem::new("spec-validate-endpoint-test"), registry)
}

fn authenticated_post(uri: &str, body: &str) -> Request<Body> {
    let mut request = Request::post(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::system(),
        ));
    request
}

#[tokio::test]
async fn validate_ioa_runs_server_cascade_without_loading_spec() {
    let app = build_router(test_state_with_registry());
    let response = app
        .oneshot(authenticated_post(
            "/api/specs/validate-ioa",
            &serde_json::json!({
                "ioa_source": ORDER_IOA,
                "sim_seeds": 1,
                "prop_test_cases": 10
            })
            .to_string(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["all_passed"], true);
}

#[tokio::test]
async fn validate_ioa_rejects_empty_source() {
    let app = build_router(test_state_with_registry());
    let response = app
        .oneshot(authenticated_post(
            "/api/specs/validate-ioa",
            &serde_json::json!({ "ioa_source": "" }).to_string(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}
