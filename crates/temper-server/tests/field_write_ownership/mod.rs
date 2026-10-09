use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use serde_json::{Value, json};
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::{
    ServerState, StorageStack, build_router,
    registry::{EntityVerificationResult, SpecRegistry, VerificationStatus},
};
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

pub const ENTITY: &str = "/tdata/Orders('owned')";
pub const SPEC: &str = r#"
[automaton]
name = "Order"
states = ["Draft"]
initial = "Draft"
strict_action_params = false
[[state]]
name = "units"
type = "counter"
initial = 7
[[state]]
name = "retry_count"
type = "counter"
initial = 8
[[state]]
name = "approved"
type = "bool"
initial = false
[[state]]
name = "pending_count"
type = "counter"
initial = 9
[[state]]
name = "is_ready"
type = "bool"
initial = true
[[state]]
name = "reviewers"
type = "list"
initial = []
[[action]]
name = "Advance"
kind = "input"
from = ["Draft"]
effect = ["units += 1", "retry_count += 2", "approved = true", "append(reviewers, 'trusted')"]
"#;

pub fn state(store: SimEventStore, strict: bool) -> ServerState {
    let csdl = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
    let spec = SPEC.replace(
        "strict_action_params = false",
        &format!("strict_action_params = {strict}"),
    );
    let mut registry = SpecRegistry::new();
    for tenant in ["acme", "other"] {
        registry.register_tenant(
            tenant,
            temper_spec::csdl::parse_csdl(csdl).unwrap(),
            csdl.into(),
            &[("Order", &spec)],
        );
        registry.set_verification_status(
            &TenantId::new(tenant),
            "Order",
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![],
                verified_at: "2026-09-06T00:00:00Z".into(),
            }),
        );
    }
    let mut state = ServerState::from_registry(ActorSystem::new("field-write-ownership"), registry);
    state.set_storage_stack(StorageStack::from_sim(store, None));
    for tenant in ["acme", "other"] {
        state
            .authz
            .reload_tenant_policies(tenant, "permit(principal, action, resource);")
            .unwrap();
    }
    state
}

pub async fn request(
    state: &ServerState,
    tenant: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    req.extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            temper_authz::SecurityContext {
                principal: temper_authz::Principal {
                    id: "writer".into(),
                    kind: temper_authz::PrincipalKind::Customer,
                    role: None,
                    acting_for: None,
                    agent_type: None,
                    attributes: Default::default(),
                },
                context_attrs: Default::default(),
                correlation_id: "field-ownership".into(),
            },
        ));
    let response = build_router(state.clone()).oneshot(req).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

pub async fn create_and_advance(state: &ServerState, tenant: &str) {
    let (status, body) = request(
        state,
        tenant,
        "POST",
        "/tdata/Orders",
        json!({"id":"owned"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = request(
        state,
        tenant,
        "POST",
        &format!("{ENTITY}/Temper.Advance"),
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}
