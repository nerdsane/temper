//! Cedar is told how each move came about.
//!
//! A direct request is `context.via == "request"`. A reaction is
//! `via == "reaction"` with the rule, the move that caused it and the chain's
//! depth. A reaction to a timer's move is `via == "timer"`, with who armed the
//! timer. The principal is the true one throughout:
//! a hook stays the hook. An app can then confine a hook to its own move with a
//! forbid narrowed to direct requests, and the reactions its move sets off, which
//! the spec declares, still run.

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
    <Schema Namespace="Temper.Cause" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Direction">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="via" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Step">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <Action Name="FindFix" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Cause.Direction"/>
      </Action>
      <Action Name="AutoGo" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Cause.Direction"/>
      </Action>
      <Action Name="Park" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Cause.Direction"/>
      </Action>
      <Action Name="FollowUp" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Cause.Direction"/>
      </Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Directions" EntityType="Temper.Cause.Direction"/>
        <EntitySet Name="Steps" EntityType="Temper.Cause.Step"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `FindFix` sets off `goes` (AutoGo on the same record), which sets off
/// `lands` (Land on the Step of the same id): a two-hop chain. `Park` arms a
/// timer whose move, `Lapse`, sets off `follows_up`.
const DIRECTION_IOA: &str = r#"
[automaton]
name = "Direction"
states = ["Open", "Found", "Going", "Parked", "Lapsed", "Followed"]
initial = "Open"
allow_indefinite_states = ["Open", "Found", "Going", "Parked", "Lapsed", "Followed"]

[[action]]
name = "FindFix"
kind = "input"
from = ["Open"]
to = "Found"

[[action.triggers]]
name = "goes"
kind = "entity"
target_entity = "Direction"
target_action = "AutoGo"
resolve_target = { kind = "same_id" }

[[action]]
name = "AutoGo"
kind = "input"
from = ["Found", "Open"]
to = "Going"

[[action.triggers]]
name = "lands"
kind = "entity"
target_entity = "Step"
target_action = "Land"
resolve_target = { kind = "same_id" }

[[action]]
name = "Park"
kind = "input"
from = ["Open"]
to = "Parked"

[[state_timeout]]
state = "Parked"
after_seconds = 1
on_timeout = "Lapse"

[[action]]
name = "Lapse"
kind = "internal"
from = ["Parked"]
to = "Lapsed"

[[action.triggers]]
name = "follows_up"
kind = "entity"
target_entity = "Direction"
target_action = "FollowUp"
resolve_target = { kind = "same_id" }

[[action]]
name = "FollowUp"
kind = "input"
from = ["Lapsed"]
to = "Followed"
"#;

const STEP_IOA: &str = r#"
[automaton]
name = "Step"
states = ["Waiting", "Landed"]
initial = "Waiting"
allow_indefinite_states = ["Waiting", "Landed"]

[[action]]
name = "Land"
kind = "input"
from = ["Waiting"]
to = "Landed"
"#;

/// Everyone may do everything, except: the hook may make only its own moves
/// (`FindFix`, `Park`) by direct request, in the fail-closed form an app writes;
/// `FollowUp` lands only as the reaction to the hook's timer; and `Land` only as
/// the second hop of the `FindFix` chain on `d3`.
const POLICY: &str = r#"
permit(principal, action, resource);

forbid(principal, action, resource)
when { principal has agent_type && principal.agent_type == "katagami-hook" }
unless {
  action == Action::"FindFix" || action == Action::"Park" ||
  (context has via && context.via != "request")
};

forbid(principal, action == Action::"FollowUp", resource)
unless {
  context has via && context.via == "timer" &&
  context has armedBy && context.armedBy == "hook-1" &&
  context has rule && context.rule == "Direction:Lapse:follows_up" &&
  context has causeAction && context.causeAction == "Lapse" &&
  context has causeEntityType && context.causeEntityType == "Direction" &&
  context has depth && context.depth == 1
};

forbid(principal, action == Action::"Land", resource)
unless {
  context has via && context.via == "reaction" &&
  context has rule && context.rule == "Direction:AutoGo:lands" &&
  context has causeAction && context.causeAction == "AutoGo" &&
  context has causeEntityType && context.causeEntityType == "Direction" &&
  context has causeEntityId && context.causeEntityId == "d3" &&
  context has depth && context.depth == 2
};
"#;

/// The webhook's principal: an agent of type `katagami-hook` acting for its owner.
fn hook() -> SecurityContext {
    SecurityContext::from_verified_jwt(
        "hook-1",
        PrincipalKind::Agent,
        Some("katagami-hook"),
        Some("owner-1"),
        None,
        None,
    )
}

/// The record's owner, making moves herself.
fn owner() -> SecurityContext {
    SecurityContext::from_verified_jwt("owner-1", PrincipalKind::Customer, None, None, None, None)
}

fn server(name: &str) -> (ServerState, SimEventStore) {
    let mut registry = SpecRegistry::new();
    let entities = [("Direction", DIRECTION_IOA), ("Step", STEP_IOA)];
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
                verified_at: "2026-10-24T00:00:00Z".to_string(),
            }),
        );
    }
    let store = SimEventStore::no_faults(91);
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state
        .authz
        .reload_tenant_policies("default", POLICY)
        .expect("policy should load");
    state.rebuild_reaction_dispatcher();
    (state, store)
}

async fn send(
    state: &ServerState,
    as_who: SecurityContext,
    mut request: Request<Body>,
) -> (StatusCode, Value) {
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

async fn post(
    state: &ServerState,
    as_who: SecurityContext,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(state, as_who, request).await
}

async fn create(state: &ServerState, set: &str, body: Value) {
    let (status, body) = post(state, owner(), &format!("/tdata/{set}"), body).await;
    assert_eq!(status, StatusCode::CREATED, "{body:?}");
}

async fn act(
    state: &ServerState,
    as_who: SecurityContext,
    id: &str,
    action: &str,
) -> (StatusCode, Value) {
    let path = format!("/tdata/Directions('{id}')/Temper.Cause.{action}");
    post(state, as_who, &path, json!({})).await
}

async fn status(state: &ServerState, entity_type: &str, id: &str) -> String {
    state
        .get_tenant_entity_state(&TenantId::default(), entity_type, id)
        .await
        .expect("record should exist")
        .state
        .status
}

/// Wait until the record reaches `to`, or give up after `within`.
async fn reach(state: &ServerState, entity_type: &str, id: &str, to: &str, within: Duration) {
    let deadline = Instant::now() + within; // determinism-ok: real-time reaction and timer test
    while status(state, entity_type, id).await != to && Instant::now() < deadline {
        // determinism-ok: real-time reaction and timer test
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time test
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hooks_move_sets_off_its_reaction_under_a_request_only_forbid() {
    let (state, _store) = server("move-cause-hook-reaction");
    create(&state, "Directions", json!({ "Id": "d1" })).await;
    create(&state, "Steps", json!({ "Id": "d1" })).await;

    let (code, body) = act(&state, hook(), "d1", "FindFix").await;
    assert_eq!(
        code,
        StatusCode::OK,
        "the hook makes its own move: {body:?}"
    );
    reach(&state, "Direction", "d1", "Going", Duration::from_secs(10)).await;
    assert_eq!(
        status(&state, "Direction", "d1").await,
        "Going",
        "the reaction the hook's move sets off runs, as the hook, past a forbid on its direct requests"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hook_cannot_pick_another_move_by_request() {
    let (state, _store) = server("move-cause-hook-request");
    // A record field named `via` does not speak for the kernel.
    create(
        &state,
        "Directions",
        json!({ "Id": "d2", "via": "reaction" }),
    )
    .await;

    let (code, body) = act(&state, hook(), "d2", "AutoGo").await;
    assert_eq!(code, StatusCode::FORBIDDEN, "{body:?}");
    let (code, body) = act(&state, hook(), "d2", "FollowUp").await;
    assert_eq!(code, StatusCode::FORBIDDEN, "{body:?}");
    assert_eq!(status(&state, "Direction", "d2").await, "Open");

    // The owner may make the same move by request.
    let (code, body) = act(&state, owner(), "d2", "AutoGo").await;
    assert_eq!(code, StatusCode::OK, "{body:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timer_armed_by_the_hook_fires_and_its_reaction_runs_as_a_timer_reaction() {
    let (state, _store) = server("move-cause-hook-timer");
    create(&state, "Directions", json!({ "Id": "t1" })).await;

    let (code, body) = act(&state, hook(), "t1", "Park").await;
    assert_eq!(code, StatusCode::OK, "{body:?}");
    // The timer's own move, Lapse, lands as its armer with no Cedar check of
    // its own. Its reaction FollowUp passes only a forbid that requires
    // `via == "timer"`, armed by the hook, at depth 1 under its rule.
    reach(
        &state,
        "Direction",
        "t1",
        "Followed",
        Duration::from_secs(15),
    )
    .await;
    assert_eq!(
        status(&state, "Direction", "t1").await,
        "Followed",
        "the timer move's reaction runs with the timer's cause"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_two_hop_chain_tells_each_hop_its_cause_and_depth() {
    let (state, _store) = server("move-cause-two-hops");
    create(&state, "Directions", json!({ "Id": "d3" })).await;
    create(&state, "Steps", json!({ "Id": "d3" })).await;

    let (code, body) = act(&state, owner(), "d3", "FindFix").await;
    assert_eq!(code, StatusCode::OK, "{body:?}");
    reach(&state, "Step", "d3", "Landed", Duration::from_secs(10)).await;
    assert_eq!(
        status(&state, "Step", "d3").await,
        "Landed",
        "the second hop passes a forbid that names its rule, cause and depth"
    );
}
