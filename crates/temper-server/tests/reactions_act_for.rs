//! A reaction with `as` is made for the user its source record names.
//!
//! A webhook's move is made by the hook, an agent acting for the app's owner
//! that the app's Cedar confines to its own moves. What the spec makes follow
//! that move runs as the hook too, so nothing in its chain is any person's.
//! A `Pusher` record is one sender's identity at GitHub: a person claims it
//! with their own move, which writes their own identity into `person` (an
//! `authenticated_subject` parameter, so no caller supplies it). The hook's
//! `Pushed` then sets off `start`, made for whoever `person` names.
//!
//! So a push on a record a person claimed starts its chain as that person,
//! and a push on an unclaimed record starts nothing: it never falls back to
//! the hook. The hook cannot claim a record itself (it would write its
//! owner's identity), and a push not made with the hook's token is refused.

use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use temper_authz::{AuthenticatedRequestContext, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Push" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Pusher">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="person" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Run">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="started_by" Type="Edm.String"/>
      </EntityType>
      <Action Name="Claim" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Push.Pusher"/>
      </Action>
      <Action Name="Pushed" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Push.Pusher"/>
      </Action>
      <Action Name="Start" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Push.Run"/>
      </Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Pushers" EntityType="Temper.Push.Pusher"/>
        <EntitySet Name="Runs" EntityType="Temper.Push.Run"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const PUSHER_IOA: &str = r#"
[automaton]
name = "Pusher"
states = ["Unclaimed", "Claimed"]
initial = "Unclaimed"
allow_indefinite_states = ["Unclaimed", "Claimed"]
strict_action_params = true

[[state]]
name = "person"
type = "string"
initial = ""

[[action]]
name = "Claim"
kind = "input"
from = ["Unclaimed"]
to = "Claimed"
params = [{ name = "person", type = "string", source = "authenticated_subject" }]

[[action]]
name = "Pushed"
kind = "input"
from = ["Unclaimed", "Claimed"]

[[action.triggers]]
name = "start"
kind = "entity"
target_entity = "Run"
target_action = "Start"
resolve_target = { kind = "same_id" }
as = "person"
"#;

/// The stand-in for an agent's run: `Start` records whose identity made it.
const RUN_IOA: &str = r#"
[automaton]
name = "Run"
states = ["Waiting", "Started"]
initial = "Waiting"
allow_indefinite_states = ["Waiting", "Started"]

[[action]]
name = "Start"
kind = "input"
from = ["Waiting"]
to = "Started"
params = [{ name = "started_by", type = "string", source = "authenticated_subject" }]
"#;

/// Everyone may do everything, except: the hook makes only its own move by
/// direct request, in the form an app writes, and what the spec makes follow
/// it may go on; and a push is made only with the hook's token. So were the
/// reaction made as the hook, Cedar would let it: only the kernel's `as`
/// decides whether the unclaimed push starts a run.
const POLICY: &str = r#"
permit(principal, action, resource);

forbid(principal, action, resource)
when { principal has agent_type && principal.agent_type == "katagami-hook" }
unless { action == Action::"Pushed" || (context has via && context.via != "request") };

forbid(principal, action == Action::"Pushed", resource)
unless { principal has agent_type && principal.agent_type == "katagami-hook" };
"#;

/// The webhook's principal, as a verified GitHub push reaches the app: an
/// agent of type `katagami-hook` acting for the app's owner.
fn hook() -> SecurityContext {
    SecurityContext::from_verified_jwt(
        "hook-github",
        PrincipalKind::Agent,
        Some("katagami-hook"),
        Some("owner-1"),
        None,
        None,
    )
}

/// A person, in person.
fn person(id: &str) -> SecurityContext {
    SecurityContext::from_verified_jwt(id, PrincipalKind::Customer, None, None, None, None)
}

fn server(name: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let entities = [("Pusher", PUSHER_IOA), ("Run", RUN_IOA)];
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("CSDL should parse"),
        CSDL.to_string(),
        &entities,
    );
    for (entity_type, _) in &entities {
        registry.set_verification_status(
            &TenantId::default(),
            entity_type,
            VerificationStatus::Completed(EntityVerificationResult {
                all_passed: true,
                levels: vec![EntityLevelSummary {
                    level: "L0 SMT".to_string(),
                    passed: true,
                    summary: "OK".to_string(),
                    details: None,
                }],
                verified_at: "2026-11-02T00:00:00Z".to_string(),
            }),
        );
    }
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(92), None));
    state
        .authz
        .reload_tenant_policies("default", POLICY)
        .expect("policy should load");
    state.rebuild_reaction_dispatcher();
    state
}

async fn post(
    state: &ServerState,
    as_who: SecurityContext,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            as_who,
        ));
    let response = build_router(state.clone()).oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 10_000_000)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A pusher and its run, both with id `id`.
async fn records(state: &ServerState, id: &str) {
    for set in ["Pushers", "Runs"] {
        let (status, body) = post(
            state,
            person("owner-1"),
            &format!("/tdata/{set}"),
            json!({ "Id": id }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body:?}");
    }
}

async fn act(state: &ServerState, as_who: SecurityContext, id: &str, action: &str) -> StatusCode {
    let path = format!("/tdata/Pushers('{id}')/Temper.Push.{action}");
    post(state, as_who, &path, json!({})).await.0
}

async fn run(state: &ServerState, id: &str) -> (String, Value) {
    let run = state
        .get_tenant_entity_state(&TenantId::default(), "Run", id)
        .await
        .expect("the run should exist")
        .state;
    let by = run.fields.get("started_by").cloned().unwrap_or(Value::Null);
    (run.status, by)
}

/// The run's status once it starts, or as it is after `within`.
async fn settle(state: &ServerState, id: &str, within: Duration) -> (String, Value) {
    let deadline = Instant::now() + within; // determinism-ok: real-time reaction test
    loop {
        let seen = run(state, id).await;
        if seen.0 == "Started" || Instant::now() >= deadline {
            // determinism-ok: real-time reaction test
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time test
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_on_a_claimed_record_starts_its_chain_as_the_claimant() {
    let state = server("acts-for-claimed");
    records(&state, "p1").await;
    assert_eq!(
        act(&state, person("ann"), "p1", "Claim").await,
        StatusCode::OK
    );

    assert_eq!(act(&state, hook(), "p1", "Pushed").await, StatusCode::OK);
    let (status, by) = settle(&state, "p1", Duration::from_secs(10)).await;
    assert_eq!(status, "Started", "the push starts the run");
    assert_eq!(
        by, "ann",
        "the run is made for the person who claimed the record, not the hook's owner"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_on_an_unclaimed_record_starts_nothing() {
    let state = server("acts-for-unclaimed");
    records(&state, "p2").await;

    assert_eq!(
        act(&state, hook(), "p2", "Pushed").await,
        StatusCode::OK,
        "the hook's own move is made"
    );
    let (status, by) = settle(&state, "p2", Duration::from_secs(2)).await;
    assert_eq!(
        (status.as_str(), by),
        ("Waiting", Value::Null),
        "no one claimed the record, so the reaction is not made, and never as the hook"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hook_cannot_claim_a_record() {
    let state = server("acts-for-hook-claim");
    records(&state, "p3").await;
    // Its claim would write the owner it acts for into `person`.
    assert_eq!(
        act(&state, hook(), "p3", "Claim").await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(act(&state, hook(), "p3", "Pushed").await, StatusCode::OK);
    let (status, _) = settle(&state, "p3", Duration::from_secs(2)).await;
    assert_eq!(status, "Waiting");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_push_without_the_hooks_token_is_refused() {
    let state = server("acts-for-unverified");
    records(&state, "p4").await;
    assert_eq!(
        act(&state, person("ann"), "p4", "Claim").await,
        StatusCode::OK
    );

    // Not made with the token the hook route mints after it checks the
    // sender's signature.
    let stranger = SecurityContext::from_verified_jwt(
        "someone",
        PrincipalKind::Agent,
        Some("other"),
        Some("ann"),
        None,
        None,
    );
    assert_eq!(
        act(&state, stranger, "p4", "Pushed").await,
        StatusCode::FORBIDDEN
    );
    let (status, _) = settle(&state, "p4", Duration::from_secs(2)).await;
    assert_eq!(status, "Waiting", "a refused push starts nothing");
}
