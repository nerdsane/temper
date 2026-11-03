//! A record created straight into a timed state arms its timer (ADR-0056).
//!
//! Creation is not a dispatch, so before this nothing armed the timer of a
//! record whose first state declares one: it waited for the record's first
//! action, and never fired for a record nothing touched. Like any arm, the
//! timer fires as whoever armed it, here the creator. From nerdsane/temper#375.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
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
    <Schema Namespace="Temper.CreationTimer" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Hold">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="fired_by" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Holds" EntityType="Temper.CreationTimer.Hold"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// A hold expires a second after it is made, unless something releases it.
const HOLD_IOA: &str = r#"
[automaton]
name = "Hold"
states = ["Waiting", "Expired", "Released"]
initial = "Waiting"
allow_indefinite_states = ["Waiting", "Expired", "Released"]
strict_action_params = true

[[state]]
name = "fired_by"
type = "string"
initial = ""

[[action]]
name = "Release"
from = ["Waiting"]
to = "Released"
params = []

[[action]]
name = "Expire"
from = ["Waiting"]
to = "Expired"
params = [{ name = "fired_by", type = "string", source = "authenticated_subject" }]

[[state_timeout]]
state = "Waiting"
after_seconds = 1
on_timeout = "Expire"
"#;

fn build_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL_XML).expect("CSDL should parse"),
        CSDL_XML.to_string(),
        &[("Hold", HOLD_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("state-timer-on-creation"), registry);
    state.registry.write().unwrap().set_verification_status(
        &TenantId::default(),
        "Hold",
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

/// `POST /tdata/Holds` as `principal`.
async fn create_hold(state: &ServerState, id: &str, principal: &str) -> StatusCode {
    let mut request = Request::post("/tdata/Holds")
        .header("Content-Type", "application/json")
        .body(Body::from(format!(r#"{{"Id":"{id}"}}"#)))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            SecurityContext::from_verified_jwt(
                principal,
                PrincipalKind::Agent,
                Some("test"),
                None,
                None,
                None,
            ),
        ));
    build_router(state.clone())
        .oneshot(request)
        .await
        .unwrap()
        .status()
}

/// The hold's status and who expired it, once it leaves `Waiting` or after
/// `within`.
async fn settle(state: &ServerState, id: &str, within: Duration) -> (String, serde_json::Value) {
    let deadline = Instant::now() + within; // determinism-ok: real-time timer test
    loop {
        let entity = state
            .get_tenant_entity_state(&TenantId::default(), "Hold", id)
            .await
            .expect("hold should exist");
        let now = Instant::now(); // determinism-ok: real-time timer test
        if entity.state.status != "Waiting" || now >= deadline {
            return (entity.state.status, entity.state.fields["fired_by"].clone());
        }
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time timer test
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_record_created_into_a_timed_state_times_out_as_its_creator() {
    let state = build_state();
    assert_eq!(
        create_hold(&state, "h1", "alice").await,
        StatusCode::CREATED
    );

    let (status, fired_by) = settle(&state, "h1", Duration::from_secs(5)).await;
    assert_eq!(
        status, "Expired",
        "nothing armed the timer of a record created into its timed state"
    );
    assert_eq!(
        fired_by,
        serde_json::json!("alice"),
        "the timer fires as the record's creator"
    );
}
