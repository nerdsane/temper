//! Strict specifications cannot be bypassed through generic writes.
#[path = "strict_generic_writes/authorization.rs"]
mod authorization;
mod common;
#[path = "strict_generic_writes/creation.rs"]
mod creation;
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
    state_with_csdl(common::CSDL_XML)
}
fn state_with_csdl(csdl: &str) -> ServerState {
    state_with_spec(csdl, SPEC)
}
fn state_with_spec(csdl: &str, spec: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(csdl).unwrap(),
        csdl.to_owned(),
        &[("Order", spec)],
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

#[tokio::test]
async fn static_specs_enforce_strict_generic_write_boundary() {
    let state = ServerState::with_specs(
        ActorSystem::new("static-strict"),
        parse_csdl(common::CSDL_XML).unwrap(),
        common::CSDL_XML.into(),
        std::collections::BTreeMap::from([("Order".into(), SPEC.into())]),
    )
    .unwrap();
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let actor = state
        .get_or_spawn_tenant_actor_with_fields(&TenantId::default(), "Order", "static", json!({}))
        .unwrap();
    let _: temper_server::entity_actor::EntityResponse = actor
        .ask(
            temper_server::entity_actor::EntityMsg::GetState,
            std::time::Duration::from_secs(2),
        )
        .await
        .unwrap();
    for method in ["PATCH", "PUT", "DELETE"] {
        let response = raw_request(
            &state,
            method,
            "/tdata/Orders('static')",
            json!({"Notes":"change"}).to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["error"]["code"],
            "StrictActionContract"
        );
    }
}

#[tokio::test]
async fn generic_pg_write_reports_unavailable_actor_system_without_panicking() {
    let mut state = state();
    state.actor_backed_types.insert("Order".into());
    for method in ["PATCH", "PUT", "DELETE"] {
        assert_eq!(
            request(
                &state,
                method,
                "/tdata/Orders('valid')",
                json!({"Notes":"change"})
            )
            .await,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
}
async fn request(
    state: &ServerState,
    method: &str,
    path: &str,
    body: serde_json::Value,
) -> StatusCode {
    raw_request(state, method, path, body.to_string())
        .await
        .status()
}
async fn raw_request(
    state: &ServerState,
    method: &str,
    path: &str,
    body: String,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("content-type", "application/json")
        .body(Body::from(body))
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
    build_router(state.clone()).oneshot(request).await.unwrap()
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
        let response = raw_request(
            &state,
            method,
            "/tdata/Orders('valid')",
            json!({"Notes":"forged"}).to_string(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error["error"]["code"], "StrictActionContract");
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

#[tokio::test]
async fn empty_action_body_is_empty_object_and_malformed_body_does_not_execute() {
    let state = state();
    request(&state, "POST", "/tdata/Orders", json!({"id":"valid"})).await;
    let response = raw_request(
        &state,
        "POST",
        "/tdata/Orders('valid')/Temper.SubmitOrder",
        "{".into(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        state
            .get_tenant_entity_state(&TenantId::default(), "Order", "valid")
            .await
            .unwrap()
            .state
            .status,
        "Draft"
    );
    let response = raw_request(
        &state,
        "POST",
        "/tdata/Orders('valid')/Temper.SubmitOrder",
        " \n".into(),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        state
            .get_tenant_entity_state(&TenantId::default(), "Order", "valid")
            .await
            .unwrap()
            .state
            .status,
        "Submitted"
    );
}

#[tokio::test]
async fn missing_strict_initializer_reports_refusal_without_creating_child() {
    let parent = SPEC.replace(
        "params = [\"Notes\"]",
        "params = [\"Notes\"]\neffect = [\"spawn('Customer', 'MissingInitializer')\"]",
    );
    let child = SPEC.replace("name = \"Order\"", "name = \"Customer\"");
    let (state, _) = common::build_single_tenant_state(
        0,
        "missing-strict-initializer",
        "default",
        &[("Order", &parent), ("Customer", &child)],
    );
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(&tenant, "Order", "parent", json!({}))
        .await
        .unwrap();
    let mut events = state.entity_observe_tx.subscribe();
    state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "parent",
            "SubmitOrder",
            json!({"Notes":"child"}),
            &Default::default(),
        )
        .await
        .unwrap();
    let refusal = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        for _ in 0..32 {
            let event = events.recv().await.unwrap();
            if event.entity_id == "parent" && event.event_name == "integration_callback_rejected" {
                assert_eq!(event.data["action"], "MissingInitializer");
                return;
            }
        }
        panic!("spawn refusal was not reported");
    })
    .await;
    assert_eq!(
        state.active_actor_count(),
        1,
        "created a child with no declared initializer"
    );
    refusal.expect("spawn refusal was not observable");
}

#[tokio::test]
async fn declared_spawn_initializes_a_strict_child_through_its_action() {
    let parent = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"
[[action]]
name = "SpawnChild"
from = ["Draft"]
to = "Submitted"
params = ["child_id", "payload", "unrelated"]
effect = ["spawn('Customer', 'Initialize', child_ref, params.child_id)"]
"#;
    let child = r#"
[automaton]
name = "Customer"
states = ["Draft", "Ready"]
initial = "Draft"
strict_action_params = true
[[state]]
name = "parent_id"
type = "string"
initial = ""
[[state]]
name = "payload"
type = "string"
initial = ""
[[action]]
name = "Initialize"
from = ["Draft"]
to = "Ready"
params = ["parent_id", "payload"]
[[action.constraints]]
kind = "param_nonempty"
param = "parent_id"
"#;
    let (state, _) = common::build_single_tenant_state(
        0,
        "strict-spawn",
        "default",
        &[("Order", parent), ("Customer", child)],
    );
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    state
        .get_or_create_tenant_entity(&TenantId::default(), "Order", "parent", json!({}))
        .await
        .unwrap();
    state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            "parent",
            "SpawnChild",
            json!({"child_id":"child","payload":"kept","unrelated":"excluded"}),
            &Default::default(),
        )
        .await
        .unwrap();
    let actual = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Ok(actual) = state
                .get_tenant_entity_state(&TenantId::default(), "Customer", "child")
                .await
                && actual.state.status == "Ready"
            {
                break actual;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("strict child initialization did not complete");
    assert_eq!(actual.state.fields["parent_id"], "parent");
    assert_eq!(actual.state.fields["payload"], "kept");
    assert!(actual.state.fields.get("unrelated").is_none());
    assert!(actual.state.fields.get("parent_type").is_none());
}
