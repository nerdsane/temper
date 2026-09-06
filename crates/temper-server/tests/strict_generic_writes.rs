//! Strict specifications cannot be bypassed through generic writes.
mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::json;
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::{
    ServerState, build_router,
    registry::{EntityVerificationResult, SpecRegistry, VerificationStatus},
};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const SPEC: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"
strict_action_params = true
[[action]]
name = "SubmitOrder"
kind = "input"
from = ["Draft"]
to = "Submitted"
params = ["Notes"]
"#;
fn state() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(common::CSDL_XML).unwrap(),
        common::CSDL_XML.to_owned(),
        &[("Order", SPEC)],
    );
    registry.set_verification_status(
        &TenantId::default(),
        "Order",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-09-06T00:00:00Z".into(),
        }),
    );
    let state = ServerState::from_registry(ActorSystem::new("strict-generic"), registry);
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    state
}
async fn request(
    state: &ServerState,
    method: &str,
    path: &str,
    body: serde_json::Value,
) -> StatusCode {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "strict-test",
                "test-agent",
                None,
            ),
        ));
    build_router(state.clone())
        .oneshot(request)
        .await
        .unwrap()
        .status()
}
#[tokio::test]
async fn generic_create_accepts_only_identity_and_initial_status() {
    let state = state();
    assert_eq!(
        request(
            &state,
            "POST",
            "/tdata/Orders",
            json!({"id":"rejected","Notes":"forged"})
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert!(!state.entity_exists(&TenantId::default(), "Order", "rejected"));
    assert_eq!(
        request(
            &state,
            "POST",
            "/tdata/Orders",
            json!({"id":"valid","Status":"Draft"})
        )
        .await,
        StatusCode::CREATED
    );
}
#[tokio::test]
async fn generic_http_mutations_cannot_bypass_actions() {
    let state = state();
    assert_eq!(
        request(&state, "POST", "/tdata/Orders", json!({"id":"valid"})).await,
        StatusCode::CREATED
    );
    for method in ["PATCH", "PUT", "DELETE"] {
        let status = request(
            &state,
            method,
            "/tdata/Orders('valid')",
            json!({"Notes":"forged"}),
        )
        .await;
        assert!(status.is_client_error(), "{method} returned {status}");
        let actual = state
            .get_tenant_entity_state(&TenantId::default(), "Order", "valid")
            .await
            .unwrap();
        assert_eq!(actual.state.status, "Draft");
        assert_ne!(actual.state.fields.get("Notes"), Some(&json!("forged")));
    }
}
#[tokio::test]
async fn direct_state_mutations_cannot_bypass_actions() {
    let state = state();
    let tenant = TenantId::default();
    assert!(
        state
            .get_or_create_tenant_entity(&tenant, "Order", "forged", json!({"Notes":"forged"}))
            .await
            .is_err()
    );
    state
        .get_or_create_tenant_entity(&tenant, "Order", "valid", json!({}))
        .await
        .unwrap();
    for replace in [false, true] {
        assert!(
            state
                .update_tenant_entity_fields(
                    &tenant,
                    "Order",
                    "valid",
                    json!({"Notes":"forged"}),
                    replace
                )
                .await
                .is_err()
        );
    }
    assert!(
        state
            .delete_tenant_entity(&tenant, "Order", "valid")
            .await
            .is_err()
    );
}
