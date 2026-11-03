//! The world the stale-write simulations run in: the specs, the policy, a
//! server on a simulated journal, and the requests the scenarios send.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use serde_json::Value;
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

pub(super) const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Stale" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Tally">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="last" Type="Edm.String"/>
        <Property Name="project_id" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Project">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <Action Name="Bump" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Stale.Tally"/>
        <Parameter Name="last" Type="Edm.String"/>
      </Action>
      <Action Name="Settle" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Stale.Tally"/>
      </Action>
      <Action Name="Repoint" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Stale.Tally"/>
        <Parameter Name="project_id" Type="Edm.String"/>
      </Action>
      <Action Name="Close" IsBound="true">
        <Parameter Name="bindingParameter" Type="Temper.Stale.Project"/>
      </Action>
      <EntityContainer Name="Container">
        <EntitySet Name="Tallies" EntityType="Temper.Stale.Tally"/>
        <EntitySet Name="Projects" EntityType="Temper.Stale.Project"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

/// `Bump` counts and writes who bumped last, so every bump changes the fields
/// Cedar reads. Cedar also sees the status of the record's project
/// (`ctx_project_status`), which `Repoint` changes. `Settle` ends the tally,
/// after which its guard refuses a `Bump`.
pub(super) const TALLY_IOA: &str = r#"
[automaton]
name = "Tally"
states = ["Open", "Settled"]
initial = "Open"
allow_indefinite_states = ["Open", "Settled"]

[[state]]
name = "count"
type = "counter"
initial = 0

[[state]]
name = "last"
type = "string"
initial = ""

[[state]]
name = "project_id"
type = "string"
initial = ""

[[context_entity]]
name = "project"
entity_type = "Project"
id_field = "project_id"

[[action]]
name = "Bump"
kind = "input"
from = ["Open"]
to = "Open"
params = ["last"]
effect = ["count += 1"]

[[action]]
name = "Repoint"
kind = "input"
from = ["Open"]
to = "Open"
params = ["project_id"]

[[action]]
name = "Settle"
kind = "input"
from = ["Open"]
to = "Settled"
"#;

pub(super) const PROJECT_IOA: &str = r#"
[automaton]
name = "Project"
states = ["Open", "Closed"]
initial = "Open"
allow_indefinite_states = ["Open", "Closed"]

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"
"#;

/// Everyone may do everything, except that Mallory may bump a tally only while
/// its project is Open.
pub(super) const POLICY: &str = r#"
permit(principal, action, resource);

forbid(principal == Customer::"mallory", action == Action::"Bump", resource)
unless { resource has ctx_project_status && resource.ctx_project_status == "Open" };
"#;

/// Seeds explored on every run, after the regressions.
pub(super) const EXPLORED_SEEDS: u64 = 48;

/// Seeds that once broke an invariant, kept forever (none yet).
pub(super) const REGRESSION_SEEDS: &[u64] = &[];

/// How many allowed moves race on one record.
pub(super) const RACING_MOVES: usize = 6;

/// How many of Mallory's moves race the record's repoint.
pub(super) const RACING_ATTACKS: usize = 3;

pub(super) fn seeds() -> impl Iterator<Item = u64> {
    let explored = (0..EXPLORED_SEEDS).filter(|seed| !REGRESSION_SEEDS.contains(seed));
    REGRESSION_SEEDS.iter().copied().chain(explored)
}

pub(super) fn customer(id: &str) -> SecurityContext {
    SecurityContext::from_verified_jwt(id, PrincipalKind::Customer, None, None, None, None)
}

/// A run: an agent acting for `owner`, as an app's run acts for its person.
pub(super) fn run(id: &str) -> SecurityContext {
    SecurityContext::from_verified_jwt(
        id,
        PrincipalKind::Agent,
        Some("run"),
        Some("owner"),
        None,
        None,
    )
}

pub(super) fn server(seed: u64, scenario: &str) -> ServerState {
    server_with_journal(seed, scenario).0
}

/// A server, and its journal to read what was written.
pub(super) fn server_with_journal(seed: u64, scenario: &str) -> (ServerState, SimEventStore) {
    let mut registry = SpecRegistry::new();
    let entities = [("Tally", TALLY_IOA), ("Project", PROJECT_IOA)];
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
                verified_at: "2026-10-27T00:00:00Z".to_string(),
            }),
        );
    }
    let store = SimEventStore::no_faults(seed);
    let name = format!("dst-stale-writes-{scenario}-{seed}");
    let mut state = ServerState::from_registry(ActorSystem::new(&name), registry);
    state.set_storage_stack(StorageStack::from_sim(store.clone(), None));
    state
        .authz
        .reload_tenant_policies("default", POLICY)
        .expect("policy should load");
    (state, store)
}

pub(super) async fn send(
    state: &ServerState,
    as_who: &str,
    request: Request<Body>,
) -> (StatusCode, Value) {
    send_as(state, customer(as_who), request).await
}

pub(super) async fn send_as(
    state: &ServerState,
    who: SecurityContext,
    mut request: Request<Body>,
) -> (StatusCode, Value) {
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(TenantId::default(), who));
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

pub(super) async fn post(
    state: &ServerState,
    as_who: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::post(path)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send(state, as_who, request).await
}

pub(super) async fn create(state: &ServerState, set: &str, body: Value) -> Result<(), String> {
    let (status, body) = post(state, "owner", &format!("/tdata/{set}"), body).await;
    if status != StatusCode::CREATED {
        return Err(format!("creating in {set}: {status} {body}"));
    }
    Ok(())
}

pub(super) async fn act(
    state: &ServerState,
    as_who: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    post(state, as_who, &format!("/tdata/{path}"), body).await
}

/// A move sent by the run `id`.
pub(super) async fn act_as_run(
    state: &ServerState,
    id: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let request = Request::post(format!("/tdata/{path}"))
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    send_as(state, run(id), request).await
}

pub(super) async fn tally(state: &ServerState, id: &str) -> temper_server::EntityState {
    state
        .get_tenant_entity_state(&TenantId::default(), "Tally", id)
        .await
        .expect("the tally should be readable")
        .state
}
