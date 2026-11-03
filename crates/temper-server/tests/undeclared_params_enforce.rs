//! With `TEMPER_UNDECLARED_PARAMS=enforce`, an action of a non-strict spec
//! changes only the fields it declares (nerdsane/temper#351, ARN-247): an
//! outside call that sends another key is refused with 400 after Cedar, and an
//! inside dispatch has the key dropped. Its own binary, so the mode, read once
//! per process, is set before anything reads it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Declared" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Note">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Notes" EntityType="Temper.Declared.Note"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// Not strict: before #351 any key of `Retitle`'s body became a field.
const NOTE_IOA: &str = r#"
[automaton]
name = "Note"
states = ["Open"]
initial = "Open"
allow_indefinite_states = ["Open"]

[[action]]
name = "Retitle"
kind = "input"
from = ["Open"]
params = ["title"]
"#;

fn build_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL should parse"),
        CSDL_XML.to_string(),
        &[("Note", NOTE_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("undeclared-params"), registry);
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Note",
        VerificationStatus::Completed(EntityVerificationResult {
            all_passed: true,
            levels: vec![EntityLevelSummary {
                level: "L0 SMT".to_string(),
                passed: true,
                summary: "OK".to_string(),
                details: None,
            }],
            verified_at: "2026-10-21T00:00:00Z".to_string(),
        }),
    );
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("policy should load");
    state
}

async fn post(state: &ServerState, path: &str, body: &str) -> (StatusCode, String) {
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
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

async fn fields(state: &ServerState, id: &str) -> serde_json::Value {
    state
        .get_tenant_entity_state(&TenantId::default(), "Note", id)
        .await
        .expect("note should exist")
        .state
        .fields
}

#[tokio::test(flavor = "multi_thread")]
async fn enforcing_refuses_outside_keys_and_drops_inside_ones() {
    // SAFETY: set before any thread reads the environment; this binary holds one test.
    unsafe { std::env::set_var("TEMPER_UNDECLARED_PARAMS", "enforce") };
    let state = build_state();
    assert_eq!(
        post(&state, "/tdata/Notes", r#"{"Id":"n1"}"#).await.0,
        StatusCode::CREATED
    );

    let (status, body) = post(
        &state,
        "/tdata/Notes('n1')/Temper.Declared.Retitle",
        r#"{"title":"Plan","owner":"mallory"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "an undeclared key from outside is refused: {body}"
    );
    assert!(
        body.contains("UndeclaredActionParams") && body.contains("owner"),
        "{body}"
    );
    assert!(
        fields(&state, "n1").await.get("owner").is_none(),
        "nothing was written"
    );

    let (status, _) = post(
        &state,
        "/tdata/Notes('n1')/Temper.Declared.Retitle",
        r#"{"Title":"Plan"}"#,
    )
    .await;
    assert!(
        status.is_success(),
        "the PascalCase alias of a declared param is accepted"
    );

    let response = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Note",
            "n1",
            "Retitle",
            serde_json::json!({ "title": "Inside", "owner": "mallory" }),
            &AgentContext::default(),
        )
        .await
        .expect("an inside dispatch runs");
    assert!(
        response.success,
        "an inside dispatch is not refused, only filtered"
    );
    let note = fields(&state, "n1").await;
    assert_eq!(note["title"], "Inside");
    assert!(
        note.get("owner").is_none(),
        "the undeclared key was dropped: {note}"
    );
}
