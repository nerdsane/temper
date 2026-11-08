//! A hook's move brings a record's armed timer forward (`fire_now_on`).
//!
//! Timers are deadlines. A change watching GitHub has a timer that looks
//! after a day: the deadline for a delivery GitHub lost, since GitHub does
//! not redeliver a failed delivery by itself. When the delivery does come,
//! the hook records it with `Heard`, and `Heard` makes that timer fire now
//! instead of a day later.
//!
//! The fire is still the timer's own: it runs as whoever armed it, and what
//! it sets off is seen by Cedar as `via == "timer"`, armed by them. The hook
//! lends it no identity and picks no move. So a person's timer brought
//! forward by the hook starts the person's run, while the hook cannot start
//! a run itself, and a timer the hook armed starts none when brought forward.
//! A `reset_on` re-arm takes the identity of the move that re-armed it, so a
//! hook move that resets a person's timer makes it the hook's.

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
    <Schema Namespace="Temper.Line" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Change">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="looked_by" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Run">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="started_by" Type="Edm.String"/>
      </EntityType>
      <Action Name="Watch" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Line.Change"/>
      </Action>
      <Action Name="Heard" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Line.Change"/>
      </Action>
      <Action Name="Nudge" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Line.Change"/>
      </Action>
      <Action Name="Start" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Line.Run"/>
      </Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Changes" EntityType="Temper.Line.Change"/>
        <EntitySet Name="Runs" EntityType="Temper.Line.Run"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Watch` enters `Watching`, which arms `Look` after a day. `Look` records
/// who it was made as and sets off `start`, the run of the same id. The hook
/// records a delivery with `Heard`, which brings the timer forward; `Nudge`
/// resets it.
const CHANGE_IOA: &str = r#"
[automaton]
name = "Change"
states = ["Waiting", "Watching", "Looked"]
initial = "Waiting"
allow_indefinite_states = ["Waiting", "Looked"]
strict_action_params = true

[[state]]
name = "looked_by"
type = "string"
initial = ""

[[action]]
name = "Watch"
kind = "input"
from = ["Waiting"]
to = "Watching"

[[action]]
name = "Heard"
kind = "input"
from = ["Watching"]

[[action]]
name = "Nudge"
kind = "input"
from = ["Watching"]

[[action]]
name = "Look"
kind = "internal"
from = ["Watching"]
to = "Looked"
params = [{ name = "looked_by", type = "string", source = "authenticated_subject" }]

[[action.triggers]]
name = "start"
kind = "entity"
target_entity = "Run"
target_action = "Start"
resolve_target = { kind = "same_id" }

# A day is the deadline for a delivery GitHub lost, not a poll: GitHub does
# not redeliver a failed delivery by itself, and a person can redeliver one
# for 3 days. A delivery that comes makes the timer fire now.
[[state_timeout]]
state = "Watching"
after_seconds = 86400
on_timeout = "Look"
reset_on = ["Nudge"]
fire_now_on = ["Heard"]
"#;

/// The stand-in for an agent's run: `Start` records whose identity made it.
const RUN_IOA: &str = r#"
[automaton]
name = "Run"
states = ["Waiting", "Started"]
initial = "Waiting"
allow_indefinite_states = ["Waiting", "Started"]
strict_action_params = true

[[state]]
name = "started_by"
type = "string"
initial = ""

[[action]]
name = "Start"
kind = "input"
from = ["Waiting"]
to = "Started"
params = [{ name = "started_by", type = "string", source = "authenticated_subject" }]
"#;

/// Everyone may do everything, except: the hook makes only its own moves by
/// direct request, and what the spec makes follow them may go on (the app's
/// rule); and a run spends a person's logins, so only a person starts one,
/// and only from a timer that person armed. In the line, the hook's run is
/// refused by the first rule, as the steps module's call to start it is a
/// request; here the second stands in for that call.
const POLICY: &str = r#"
permit(principal, action, resource);

forbid(principal, action, resource)
when { principal has agent_type && principal.agent_type == "app-hook" }
unless {
  action == Action::"Watch" || action == Action::"Heard" || action == Action::"Nudge" ||
  (context has via && context.via != "request")
};

forbid(principal, action == Action::"Start", resource)
unless {
  principal is Customer &&
  context has via && context.via == "timer" &&
  context has armedBy && context.armedBy == principal.id
};
"#;

/// The webhook's principal, as a verified GitHub delivery reaches the app:
/// an agent of type `app-hook` acting for the app's owner.
fn hook() -> SecurityContext {
    SecurityContext::from_verified_jwt(
        "hook-github",
        PrincipalKind::Agent,
        Some("app-hook"),
        Some("owner-1"),
        None,
        None,
    )
}

/// A person, in person.
fn person(id: &str) -> SecurityContext {
    SecurityContext::from_verified_jwt(id, PrincipalKind::Customer, None, None, None, None)
}

#[path = "timer_fire_now/dst.rs"]
mod dst;
#[path = "dst_state_timers_follow_state/process_clock.rs"]
mod process_clock;

fn server(name: &str) -> ServerState {
    server_with_spec(name, CHANGE_IOA, SimEventStore::no_faults(93))
}

fn server_with_spec(name: &str, change: &str, events: SimEventStore) -> ServerState {
    let mut registry = SpecRegistry::new();
    let entities = [("Change", change), ("Run", RUN_IOA)];
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
                verified_at: "2026-11-03T00:00:00Z".to_string(),
            }),
        );
    }
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(events, None));
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

/// A change and its run, both with id `id`.
async fn records(state: &ServerState, id: &str) {
    for set in ["Changes", "Runs"] {
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

async fn act(state: &ServerState, as_who: SecurityContext, id: &str, action: &str) {
    let path = format!("/tdata/Changes('{id}')/Temper.Line.{action}");
    let (status, body) = post(state, as_who, &path, json!({})).await;
    assert_eq!(status, StatusCode::OK, "{action}: {body:?}");
}

/// A record's status and one of its fields.
async fn read(state: &ServerState, entity_type: &str, id: &str, field: &str) -> (String, Value) {
    let record = state
        .get_tenant_entity_state(&TenantId::default(), entity_type, id)
        .await
        .expect("the record should exist")
        .state;
    let value = record.fields.get(field).cloned().unwrap_or(Value::Null);
    (record.status, value)
}

/// The change once its timer fired, with who it fired as, or as it is after
/// `within`. Its timer is a day long, so it fires within seconds only when
/// brought forward.
async fn looked(state: &ServerState, id: &str, within: Duration) -> (String, Value) {
    let deadline = Instant::now() + within; // determinism-ok: real-time timer test
    loop {
        let seen = read(state, "Change", id, "looked_by").await;
        if seen.0 == "Looked" || Instant::now() >= deadline {
            // determinism-ok: real-time timer test
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time test
    }
}

/// The run's status and who started it, once it starts or after `within`.
async fn run(state: &ServerState, id: &str, within: Duration) -> (String, Value) {
    let deadline = Instant::now() + within; // determinism-ok: real-time reaction test
    loop {
        let seen = read(state, "Run", id, "started_by").await;
        if seen.0 == "Started" || Instant::now() >= deadline {
            // determinism-ok: real-time reaction test
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time test
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_hooks_move_brings_a_persons_timer_forward_and_it_fires_as_the_person() {
    let state = server("fire-now-person");
    records(&state, "c1").await;
    act(&state, person("ann"), "c1", "Watch").await;

    act(&state, hook(), "c1", "Heard").await;
    let (status, by) = looked(&state, "c1", Duration::from_secs(10)).await;
    assert_eq!(
        status, "Looked",
        "the delivery makes the day-long timer fire now"
    );
    assert_eq!(by, "ann", "the timer fires as the person who armed it");
    // Start passes only as a person, under a timer that person armed.
    let (status, by) = run(&state, "c1", Duration::from_secs(10)).await;
    assert_eq!(
        (status.as_str(), by),
        ("Started", json!("ann")),
        "the person's run starts"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn the_hook_cannot_start_a_run_itself() {
    let state = server("fire-now-hook-direct");
    records(&state, "c2").await;
    let (status, _) = post(
        &state,
        hook(),
        "/tdata/Runs('c2')/Temper.Line.Start",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(
        run(&state, "c2", Duration::ZERO).await.0,
        "Waiting",
        "a refused start starts nothing"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timer_the_hook_armed_fires_as_the_hook_and_starts_no_run() {
    let state = server("fire-now-hook-armed");
    records(&state, "c3").await;
    act(&state, hook(), "c3", "Watch").await;

    act(&state, hook(), "c3", "Heard").await;
    let (status, by) = looked(&state, "c3", Duration::from_secs(10)).await;
    assert_eq!(
        status, "Looked",
        "the hook's own timer is brought forward too"
    );
    assert_eq!(
        by, "owner-1",
        "it fires as the hook, which acts for the owner, not as any person in person"
    );
    let (status, by) = run(&state, "c3", Duration::from_secs(2)).await;
    assert_eq!(
        (status.as_str(), by),
        ("Waiting", json!("")),
        "the hook's timer starts no run"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reset_by_the_hook_makes_the_timer_the_hooks() {
    let state = server("fire-now-hook-reset");
    records(&state, "c4").await;
    act(&state, person("ann"), "c4", "Watch").await;

    // A `reset_on` move re-arms the timer as its own caller: the hook.
    act(&state, hook(), "c4", "Nudge").await;
    act(&state, hook(), "c4", "Heard").await;
    let (status, by) = looked(&state, "c4", Duration::from_secs(10)).await;
    assert_eq!(status, "Looked");
    assert_eq!(
        by, "owner-1",
        "the reset took the hook's identity, not ann's"
    );
    let (status, _) = run(&state, "c4", Duration::from_secs(2)).await;
    assert_eq!(status, "Waiting", "so it starts no run");
}
