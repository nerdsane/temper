//! A refused action answers its message as before and, beside it, the typed
//! reason: the failed guard and its conditions, the state and the states the
//! action starts from, or the parameter. Callers read fields, not the message.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Refusal" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Lock">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Locks" EntityType="Temper.Refusal.Lock"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const LOCK_IOA: &str = r#"
[automaton]
name = "Lock"
states = ["Free", "Held"]
initial = "Free"
allow_indefinite_states = ["Free", "Held"]

[[state]]
name = "branch"
type = "string"
initial = ""

[[state]]
name = "holders"
type = "counter"
initial = 0

[[state]]
name = "waiting"
type = "bool"
initial = false

[[action]]
name = "Move"
kind = "input"
from = ["Free"]
params = ["branch"]

[[action]]
name = "Join"
kind = "input"
from = ["Free"]
effect = ["holders += 1", "waiting = true"]

[[action]]
name = "Pickup"
kind = "input"
from = ["Free"]
guard = "branch == 'Harbour'"

[[action]]
name = "Propose"
kind = "input"
from = ["Free"]
guard = "!waiting"

[[action]]
name = "Acquire"
kind = "input"
from = ["Free"]
to = "Held"
guard = "holders <= 1"

[[action]]
name = "Either"
kind = "input"
from = ["Free"]
guard = "holders <= 1 || !waiting"

[[action]]
name = "Release"
kind = "input"
from = ["Held"]
to = "Free"

[[action]]
name = "Claim"
kind = "input"
from = ["Free"]
params = ["holder"]
[[action.constraints]]
kind = "param_nonempty"
param = "holder"
"#;

fn build_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL should parse"),
        CSDL_XML.to_string(),
        &[("Lock", LOCK_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("refusal-detail"), registry);
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Lock",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0 SMT".to_string(),
                passed: true,
                summary: "OK".to_string(),
                details: None,
            }],
            verified_at: "2026-10-23T00:00:00Z".to_string(),
        }),
    );
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("policy should load");
    state
}

async fn post(state: &ServerState, path: &str, body: Value) -> (StatusCode, Value) {
    let mut request = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_verified_jwt(
                "alice",
                PrincipalKind::Agent,
                Some("test"),
                None,
                None,
                None,
            ),
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, body)
}

async fn act(state: &ServerState, action: &str, body: Value) -> (StatusCode, Value) {
    post(
        state,
        &format!("/tdata/Locks('l1')/Temper.Refusal.{action}"),
        body,
    )
    .await
}

/// A lock at branch `Market Street, x = 1` with two holders and someone waiting.
async fn busy_lock() -> ServerState {
    let state = build_state();
    let (status, body) = post(&state, "/tdata/Locks", json!({"Id": "l1"})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = act(&state, "Move", json!({"branch": "Market Street, x = 1"})).await;
    assert!(status.is_success(), "{body}");
    for _ in 0..2 {
        let (status, body) = act(&state, "Join", json!({})).await;
        assert!(status.is_success(), "{body}");
    }
    state
}

fn assert_refusal(status: StatusCode, body: &Value, expected: Value) {
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let error = &body["error"];
    for (key, value) in expected.as_object().unwrap() {
        assert_eq!(&error[key], value, "field `{key}` of {body}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn guard_refusals_carry_the_failed_guard_and_its_typed_conditions() {
    let state = busy_lock().await;

    let (status, body) = act(&state, "Pickup", json!({})).await;
    assert_refusal(
        status,
        &body,
        json!({
            "code": "GuardFailed",
            "message": "Action 'Pickup' blocked from state 'Free': guard 'branch == 'Harbour'' failed (branch = Market Street, x = 1)",
            "guard": "branch == 'Harbour'",
            "conditions": [
                {"field": "branch", "op": "==", "wants": "Harbour", "value": "Market Street, x = 1"}
            ],
        }),
    );

    let (status, body) = act(&state, "Propose", json!({})).await;
    assert_refusal(
        status,
        &body,
        json!({
            "code": "GuardFailed",
            "guard": "!waiting",
            "conditions": [{"field": "waiting", "op": "==", "wants": false, "value": true}],
        }),
    );

    let (status, body) = act(&state, "Acquire", json!({})).await;
    assert_refusal(
        status,
        &body,
        json!({
            "code": "GuardFailed",
            "message": "Action 'Acquire' blocked from state 'Free': guard 'holders <= 1' failed (holders = 2)",
            "guard": "holders <= 1",
            "conditions": [{"field": "holders", "op": "<=", "wants": 1, "value": 2}],
        }),
    );

    // An `||` does not split into conditions; the guard is named whole.
    let (status, body) = act(&state, "Either", json!({})).await;
    assert_refusal(
        status,
        &body,
        json!({
            "code": "GuardFailed",
            "guard": "holders <= 1 || !waiting",
            "conditions": [],
        }),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_wrong_state_names_the_state_and_where_the_action_starts() {
    let state = busy_lock().await;
    let (status, body) = act(&state, "Release", json!({})).await;
    assert_refusal(
        status,
        &body,
        json!({
            "code": "InvalidState",
            "message": "Action 'Release' not valid from state 'Free'",
            "state": "Free",
            "from": ["Held"],
        }),
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn parameter_refusals_name_the_parameter() {
    let state = busy_lock().await;
    let (status, body) = act(&state, "Claim", json!({})).await;
    assert_refusal(
        status,
        &body,
        json!({
            "code": "MissingParameter",
            "message": "Action 'Claim' requires parameter 'holder'",
            "parameter": "holder",
        }),
    );
    let (status, body) = act(&state, "Claim", json!({"holder": " "})).await;
    assert_refusal(
        status,
        &body,
        json!({
            "code": "ParameterConstraint",
            "message": "Action 'Claim' constraint failed for parameter 'holder'",
            "parameter": "holder",
        }),
    );
}

#[tokio::test]
async fn a_checked_preview_preserves_the_interpreters_typed_refusal() {
    let state = busy_lock().await;
    let (status, original) = act(&state, "Pickup", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(original["error"]["code"], "GuardFailed");
    let verified = state
        .registry
        .read()
        .unwrap()
        .get_verification_status(&TenantId::default(), "Lock")
        .cloned()
        .unwrap();
    // Adding a record constraint makes the same request take the outside
    // preview path. Its refusal must retain the interpreter's structured data.
    let ioa = format!(
        "{LOCK_IOA}\n[[field_invariant]]\nname = 'NonnegativeHolders'\nassert = 'holders >= 0'\n"
    );
    state.registry.write().unwrap().register_tenant(
        "default",
        parse_csdl(CSDL_XML).unwrap(),
        CSDL_XML.into(),
        &[("Lock", &ioa)],
    );
    state
        .registry
        .write()
        .unwrap()
        .set_verification_status(&TenantId::default(), "Lock", verified);
    let (status, previewed) = act(&state, "Pickup", json!({})).await;
    assert_refusal(status, &previewed, original["error"].clone());
}
