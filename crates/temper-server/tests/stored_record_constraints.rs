//! Relation integrity, field invariants and hard cross-invariants hold for
//! the record a write from outside the kernel stores: OData create, PATCH,
//! PUT and bound actions. The kernel's own follow-ons are not held to them,
//! so they can finish what an earlier move started.
//!
//! Fixture: a `Session` points at an `Environment` through `EnvironmentId`
//! (a CSDL referential constraint). A hard cross-invariant keeps a session
//! out of an archived environment, and field invariants require a note on a
//! closed or high-priority session.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const CSDL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Rec" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Environment">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Name" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Session">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="EnvironmentId" Type="Edm.String" Nullable="false"/>
        <Property Name="Note" Type="Edm.String"/>
        <Property Name="Priority" Type="Edm.String"/>
        <NavigationProperty Name="Environment" Type="Temper.Rec.Environment">
          <ReferentialConstraint Property="EnvironmentId" ReferencedProperty="Id"/>
        </NavigationProperty>
      </EntityType>
      <EntityContainer Name="RecService">
        <EntitySet Name="Environments" EntityType="Temper.Rec.Environment"/>
        <EntitySet Name="Sessions" EntityType="Temper.Rec.Session"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const ENVIRONMENT_IOA: &str = r#"
[automaton]
name = "Environment"
states = ["Active", "Archived"]
initial = "Active"

[[action]]
name = "Archive"
kind = "input"
from = ["Active"]
to = "Archived"
"#;

const SESSION_IOA: &str = r#"
[automaton]
name = "Session"
states = ["Open", "Closed"]
initial = "Open"

[[action]]
name = "Move"
kind = "input"
from = ["Open"]
params = ["EnvironmentId"]

[[action]]
name = "Annotate"
kind = "input"
from = ["Open", "Closed"]
params = ["Note", "Priority"]

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[field_invariant]]
name = "ClosedSessionsCarryANote"
message = "a closed session carries a note"
assert = "status == 'Closed' => Note != null"

[[field_invariant]]
name = "HighPrioritySessionsCarryANote"
message = "a high-priority session carries a note"
assert = "Priority == 'High' => Note != null"
"#;

const CROSS_INVARIANTS: &str = r#"
version = 1

[[invariant]]
name = "SessionEnvironmentNotArchived"
kind = "hard"
on = "Session.*"
assert = 'related(Environment, EnvironmentId).status not in ["Archived"]'
"#;

fn verified() -> VerificationStatus {
    VerificationStatus::Completed(EntityVerificationResult {
        all_passed: true,
        levels: vec![EntityLevelSummary {
            level: "L0 SMT".to_string(),
            passed: true,
            summary: "OK".to_string(),
            details: None,
        }],
        verified_at: "2026-10-23T00:00:00Z".to_string(),
    })
}

fn build_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant_with_constraints(
        "default",
        parse_csdl(CSDL).expect("CSDL parse"),
        CSDL.to_string(),
        &[("Environment", ENVIRONMENT_IOA), ("Session", SESSION_IOA)],
        Some(CROSS_INVARIANTS.to_string()),
    );
    let state = ServerState::from_registry(ActorSystem::new("stored-record-test"), registry);
    {
        let mut registry = state.registry.write().unwrap();
        for entity_type in ["Environment", "Session"] {
            registry.set_verification_status(&TenantId::default(), entity_type, verified());
        }
    }
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("fixture policy should parse");
    state
}

fn caller() -> temper_authz::SecurityContext {
    temper_authz::SecurityContext::from_resolved_identity("stored-record-test", "test-agent", None)
}

async fn send(state: &ServerState, method: Method, uri: &str, body: Value) -> (StatusCode, Value) {
    send_as(state, method, uri, body, caller()).await
}

async fn send_as(
    state: &ServerState,
    method: Method,
    uri: &str,
    body: Value,
    security_ctx: temper_authz::SecurityContext,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            security_ctx,
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1_000_000)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn stored_session(state: &ServerState, id: &str) -> (String, Value) {
    let response = state
        .get_tenant_entity_state(&TenantId::default(), "Session", id)
        .await
        .expect("session should be readable");
    (response.state.status, response.state.fields)
}

/// Environments `env-a` and `env-b` are active, `env-old` is archived, and
/// session `s1` is open in `env-a`.
async fn seeded() -> ServerState {
    let state = build_state();
    for id in ["env-a", "env-b", "env-old"] {
        let (status, body) = send(
            &state,
            Method::POST,
            "/tdata/Environments",
            json!({"Id": id}),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "seed {id}: {body}");
    }
    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Environments('env-old')/Temper.Rec.Archive",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "archive env-old: {body}");
    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Sessions",
        json!({"Id": "s1", "EnvironmentId": "env-a"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed s1: {body}");
    state
}

async fn bound_action(state: &ServerState, action: &str, body: Value) -> (StatusCode, Value) {
    send(
        state,
        Method::POST,
        &format!("/tdata/Sessions('s1')/Temper.Rec.{action}"),
        body,
    )
    .await
}

#[tokio::test]
async fn a_bound_action_cannot_point_a_record_at_a_missing_target() {
    let state = seeded().await;
    let (status, body) =
        bound_action(&state, "Move", json!({"EnvironmentId": "env-missing"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (_, fields) = stored_session(&state, "s1").await;
    assert_eq!(fields["EnvironmentId"], "env-a");
}

#[tokio::test]
async fn a_bound_action_cannot_move_a_record_under_an_archived_parent() {
    let state = seeded().await;
    let (status, body) = bound_action(&state, "Move", json!({"EnvironmentId": "env-old"})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (_, fields) = stored_session(&state, "s1").await;
    assert_eq!(fields["EnvironmentId"], "env-a");
}

#[tokio::test]
async fn a_bound_action_is_checked_against_the_status_it_moves_to() {
    let state = seeded().await;
    let (status, body) = bound_action(&state, "Close", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(stored_session(&state, "s1").await.0, "Open");

    let (status, body) = bound_action(&state, "Annotate", json!({"Note": "done"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = bound_action(&state, "Close", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(stored_session(&state, "s1").await.0, "Closed");
}

#[tokio::test]
async fn a_valid_bound_action_writes_its_fields() {
    let state = seeded().await;
    let (status, body) = bound_action(&state, "Move", json!({"EnvironmentId": "env-b"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, fields) = stored_session(&state, "s1").await;
    assert_eq!(fields["EnvironmentId"], "env-b");
}

#[tokio::test]
async fn a_kernel_follow_on_finishes_on_a_record_whose_parent_was_archived() {
    let state = seeded().await;
    let (status, body) = bound_action(&state, "Annotate", json!({"Note": "wrapping up"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Environments('env-a')/Temper.Rec.Archive",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "archive env-a: {body}");

    // An outside call is held to the cross-invariant.
    let (status, body) = bound_action(&state, "Close", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(stored_session(&state, "s1").await.0, "Open");

    // The kernel's own follow-on still ends the session.
    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Session",
            "s1",
            "Close",
            json!({}),
            &AgentContext::system(),
        )
        .await
        .expect("dispatch should answer");
    assert!(response.success, "{:?}", response.error);
    assert_eq!(stored_session(&state, "s1").await.0, "Closed");
}

#[tokio::test]
async fn a_module_request_back_into_the_api_counts_as_a_kernel_follow_on() {
    let state = seeded().await;
    let (status, body) = bound_action(&state, "Annotate", json!({"Note": "wrapping up"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Environments('env-a')/Temper.Rec.Archive",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "archive env-a: {body}");

    // The kernel names the module on the credential it hands a WASM module.
    let mut module_ctx = caller();
    module_ctx
        .context_attrs
        .insert("module".to_string(), json!("session_closer"));
    let (status, body) = send_as(
        &state,
        Method::POST,
        "/tdata/Sessions('s1')/Temper.Rec.Close",
        json!({}),
        module_ctx.clone(),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(stored_session(&state, "s1").await.0, "Closed");

    // The same holds for a record the module creates.
    let (status, body) = send_as(
        &state,
        Method::POST,
        "/tdata/Sessions",
        json!({"Id": "s-closing", "EnvironmentId": "env-a"}),
        module_ctx,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Sessions",
        json!({"Id": "s-outside", "EnvironmentId": "env-a"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}

#[tokio::test]
async fn a_patch_is_checked_on_its_top_level_fields() {
    let state = seeded().await;
    // A nested object named "fields" is stored as data; the invariant reads
    // the record's own Priority and Note.
    let (status, body) = send(
        &state,
        Method::PATCH,
        "/tdata/Sessions('s1')",
        json!({"Priority": "High", "fields": {"Note": "nested"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (_, fields) = stored_session(&state, "s1").await;
    assert_ne!(fields["Priority"], "High");
}

#[tokio::test]
async fn a_patch_is_checked_against_the_entitys_own_status() {
    let state = seeded().await;
    let (status, body) = bound_action(&state, "Annotate", json!({"Note": "done"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = bound_action(&state, "Close", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The body's Status is not stored, so it does not decide the check.
    let (status, body) = send(
        &state,
        Method::PATCH,
        "/tdata/Sessions('s1')",
        json!({"Status": "Open", "Note": null}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(stored_session(&state, "s1").await.1["Note"], "done");
}

#[tokio::test]
async fn a_put_is_checked_against_the_entitys_own_status() {
    let state = seeded().await;
    let (status, body) = bound_action(&state, "Annotate", json!({"Note": "done"})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = bound_action(&state, "Close", json!({})).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let (status, body) = send(
        &state,
        Method::PUT,
        "/tdata/Sessions('s1')",
        json!({"EnvironmentId": "env-a"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(stored_session(&state, "s1").await.1["Note"], "done");
}

#[tokio::test]
async fn a_create_is_checked_on_its_top_level_fields() {
    let state = seeded().await;
    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Sessions",
        json!({"Id": "s2", "EnvironmentId": "env-a", "Priority": "High", "fields": {"Note": "nested"}}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
}
