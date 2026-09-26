mod common;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::json;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{EntityVerificationResult, VerificationStatus};
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

#[tokio::test]
async fn collection_creation_authorizes_before_exposing_strict_contract_details() {
    let (state, _) = common::build_single_tenant_state(
        0,
        "strict-create-authority",
        "default",
        &[("Order", SPEC)],
    );
    state
        .authz
        .reload_tenant_policies("default", "forbid(principal, action, resource);")
        .unwrap();
    assert_denied_creates(&state).await;
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Order",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![],
            verified_at: "2026-09-07T00:00:00Z".into(),
        }),
    );
    assert_denied_creates(&state).await;
}

async fn assert_denied_creates(state: &temper_server::ServerState) {
    let router = temper_server::build_router(state.clone())
        .layer(axum::middleware::from_fn(unauthorized_fixture_identity));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/tdata/Orders", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
        .unwrap();
    for body in [
        json!({"Id":"denied"}),
        json!({"Id":"denied", "Notes":"forged"}),
        json!({"Id":"denied", "Status":"Submitted"}),
    ] {
        let response = client.post(&url).json(&body).send().await.unwrap();
        let status = response.status();
        let error: serde_json::Value = response.json().await.unwrap();
        assert_eq!(status, StatusCode::FORBIDDEN, "{error}");
        assert_eq!(error["error"]["code"], "AuthorizationDenied");
        assert_eq!(state.active_actor_count(), 0);
    }
    server.abort();
}

async fn unauthorized_fixture_identity(
    mut request: Request<Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    // Identity is a fixture; HTTP, Cedar and creation handling are real.
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            temper_authz::SecurityContext::from_resolved_identity(
                "unauthorized-test",
                "test",
                None,
            ),
        ));
    next.run(request).await
}
#[tokio::test]
async fn invalid_creation_fields_are_rejected_before_and_after_cache_population() {
    let (state, _) =
        common::build_single_tenant_state(0, "strict-cached-create", "default", &[("Order", SPEC)]);
    let tenant = TenantId::default();
    assert!(
        state
            .get_or_create_tenant_entity(&tenant, "Order", "valid", json!({"Notes":"forbidden"}))
            .await
            .is_err()
    );
    assert_eq!(state.active_actor_count(), 0);
    state
        .get_or_create_tenant_entity(&tenant, "Order", "valid", json!({}))
        .await
        .unwrap();
    assert!(
        state
            .get_or_create_tenant_entity(&tenant, "Order", "valid", json!({"Notes":"forbidden"}))
            .await
            .is_err()
    );
    let valid = state
        .get_or_create_tenant_entity(&tenant, "Order", "valid", json!({}))
        .await
        .unwrap();
    assert!(valid.state.fields.get("Notes").is_none());
    assert_eq!(state.active_actor_count(), 1);
}
