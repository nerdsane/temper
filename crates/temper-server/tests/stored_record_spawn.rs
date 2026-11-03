//! An outside action that spawns a child is checked on the record it stores,
//! including the child's id in the field the spawn writes. The kernel creates
//! the child after the parent commits, so the relation and the hard
//! cross-invariant that read the child read it as the spawn will create it:
//! its first state with its initial action applied.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const CSDL: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Spawn" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Child">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityType Name="Run">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="ChildRef" Type="Edm.String" Nullable="true"/>
        <NavigationProperty Name="Child" Type="Temper.Spawn.Child">
          <ReferentialConstraint Property="ChildRef" ReferencedProperty="Id"/>
        </NavigationProperty>
      </EntityType>
      <EntityContainer Name="SpawnService">
        <EntitySet Name="Children" EntityType="Temper.Spawn.Child"/>
        <EntitySet Name="Runs" EntityType="Temper.Spawn.Run"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const CHILD_IOA: &str = r#"
[automaton]
name = "Child"
states = ["New", "Initialized", "Archived"]
initial = "New"

[[action]]
name = "Init"
kind = "input"
from = ["New"]
to = "Initialized"

[[action]]
name = "InitArchived"
kind = "input"
from = ["New"]
to = "Archived"

[[action]]
name = "Archive"
kind = "input"
from = ["New", "Initialized"]
to = "Archived"

[[action]]
name = "Join"
kind = "input"
from = ["New"]
to = "Initialized"
guard = "Run[run_id].status in ['Running']"
"#;

const RUN_IOA: &str = r#"
[automaton]
name = "Run"
states = ["Idle", "Running"]
initial = "Idle"

[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Running"
params = ["child_id"]
effect = ["spawn('Child', 'Init', ChildRef, params.child_id)"]

[[action]]
name = "StartArchived"
kind = "input"
from = ["Idle"]
to = "Running"
params = ["child_id"]
effect = ["spawn('Child', 'InitArchived', ChildRef, params.child_id)"]

[[action]]
name = "Launch"
kind = "input"
from = ["Idle"]
to = "Running"
params = ["child_id"]
effect = ["spawn('Child', 'Join', ChildRef, params.child_id)"]

[[action]]
name = "ArchiveThenInit"
kind = "input"
from = ["Idle"]
to = "Running"
params = ["child_id"]
effect = ["spawn('Child', 'Archive', ChildRef, params.child_id)", "spawn('Child', 'Init', ChildRef, params.child_id)"]

[[action]]
name = "InitThenArchive"
kind = "input"
from = ["Idle"]
to = "Running"
params = ["child_id"]
effect = ["spawn('Child', 'Init', ChildRef, params.child_id)", "spawn('Child', 'Archive', ChildRef, params.child_id)"]

[[action]]
name = "ArchiveThenInitAllowed"
kind = "input"
from = ["Idle"]
to = "Running"
params = ["child_id"]
effect = ["spawn('Child', 'Archive', ChildRef, params.child_id)", "spawn('Child', 'Init', ChildRef, params.child_id)"]

[[field_invariant]]
name = "RunningRunsHaveAChild"
message = "a running run names its child"
assert = "status == 'Running' => ChildRef != null"
"#;

const CROSS_INVARIANTS: &str = r#"
version = 1

[[invariant]]
name = "RepeatedSpawnChildInitialized"
kind = "hard"
on = "Run.ArchiveThenInit"
assert = 'related(Child, ChildRef).status in ["Initialized"]'

[[invariant]]
name = "RepeatedSpawnChildArchived"
kind = "hard"
on = "Run.InitThenArchive"
assert = 'related(Child, ChildRef).status in ["Archived"]'

[[invariant]]
name = "RefusedInitializerLeavesArchivedChild"
kind = "hard"
on = "Run.ArchiveThenInitAllowed"
assert = 'related(Child, ChildRef).status in ["Archived"]'

[[invariant]]
name = "StartedRunChildNotArchived"
kind = "hard"
on = "Run.Start"
assert = 'related(Child, ChildRef).status not in ["Archived"]'

[[invariant]]
name = "StartedArchivedRunChildNotArchived"
kind = "hard"
on = "Run.StartArchived"
assert = 'related(Child, ChildRef).status not in ["Archived"]'

[[invariant]]
name = "LaunchedRunChildInitialized"
kind = "hard"
on = "Run.Launch"
assert = 'related(Child, ChildRef).status in ["Initialized"]'
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
    build_state_with_specs(CHILD_IOA, RUN_IOA, CROSS_INVARIANTS)
}

fn build_state_with_specs(child: &str, run: &str, cross_invariants: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant_with_constraints(
        "default",
        parse_csdl(CSDL).expect("CSDL parse"),
        CSDL.to_string(),
        &[("Child", child), ("Run", run)],
        Some(cross_invariants.to_string()),
    );
    let state = ServerState::from_registry(ActorSystem::new("stored-record-spawn"), registry);
    {
        let mut registry = state.registry.write().unwrap();
        for entity_type in ["Child", "Run"] {
            registry.set_verification_status(&TenantId::default(), entity_type, verified());
        }
    }
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("fixture policy should parse");
    state
}

async fn send(state: &ServerState, method: Method, uri: &str, body: Value) -> (StatusCode, Value) {
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
            temper_authz::SecurityContext::from_resolved_identity(
                "stored-record-spawn",
                "test-agent",
                None,
            ),
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

#[tokio::test]
async fn an_action_that_starts_a_child_is_checked_with_the_childs_id() {
    let state = build_state();
    let (status, body) = send(&state, Method::POST, "/tdata/Runs", json!({"Id": "r1"})).await;
    assert_eq!(status, StatusCode::CREATED, "seed r1: {body}");

    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Runs('r1')/Temper.Spawn.Start",
        json!({"child_id": "c1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let run = state
        .get_tenant_entity_state(&TenantId::default(), "Run", "r1")
        .await
        .expect("run should be readable");
    assert_eq!(run.state.status, "Running");
    assert_eq!(run.state.fields["ChildRef"], "c1");
}

async fn stored_run(state: &ServerState, id: &str) -> (String, Value) {
    let run = state
        .get_tenant_entity_state(&TenantId::default(), "Run", id)
        .await
        .expect("run should be readable");
    (run.state.status, run.state.fields["ChildRef"].clone())
}

/// A run's first child: the run names no child before the action, and the
/// child it starts begins initialized, so the cross-invariant holds.
#[tokio::test]
async fn a_first_spawn_is_checked_against_the_child_it_creates() {
    let state = build_state();
    let (status, body) = send(&state, Method::POST, "/tdata/Runs", json!({"Id": "r1"})).await;
    assert_eq!(status, StatusCode::CREATED, "seed r1: {body}");

    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Runs('r1')/Temper.Spawn.Start",
        json!({"child_id": "c1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        stored_run(&state, "r1").await,
        ("Running".to_string(), json!("c1"))
    );
}

/// A run that names an archived child starts a new one: the check reads the
/// new child, not the one the run named before.
#[tokio::test]
async fn a_changed_child_reference_is_checked_against_the_new_child() {
    let state = build_state();
    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Children",
        json!({"Id": "old"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed old: {body}");
    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Children('old')/Temper.Spawn.Archive",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "archive old: {body}");
    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Runs",
        json!({"Id": "r1", "ChildRef": "old"}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "seed r1: {body}");

    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Runs('r1')/Temper.Spawn.Start",
        json!({"child_id": "new"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        stored_run(&state, "r1").await,
        ("Running".to_string(), json!("new"))
    );
}

/// The child is read as the spawn leaves it: a new child whose initial action
/// archives it, and an existing archived child its initial action cannot
/// move, both break the cross-invariant.
#[tokio::test]
async fn a_spawn_is_refused_when_the_child_it_leaves_breaks_the_invariant() {
    let state = build_state();
    for id in ["r1", "r2"] {
        let (status, body) = send(&state, Method::POST, "/tdata/Runs", json!({"Id": id})).await;
        assert_eq!(status, StatusCode::CREATED, "seed {id}: {body}");
    }

    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Runs('r1')/Temper.Spawn.StartArchived",
        json!({"child_id": "c1"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.to_string()
            .contains("Child('c1') has status='Archived'"),
        "{body}"
    );
    assert_eq!(stored_run(&state, "r1").await.0, "Idle");

    let (status, body) = send(&state, Method::POST, "/tdata/Children", json!({"Id": "c2"})).await;
    assert_eq!(status, StatusCode::CREATED, "seed c2: {body}");
    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Children('c2')/Temper.Spawn.Archive",
        json!({}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "archive c2: {body}");
    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Runs('r2')/Temper.Spawn.Start",
        json!({"child_id": "c2"}),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.to_string()
            .contains("Child('c2') has status='Archived'"),
        "{body}"
    );
    assert_eq!(stored_run(&state, "r2").await.0, "Idle");
}

/// The child's initial action runs after the parent commits, so a guard on
/// the parent reads the parent as the spawning action leaves it.
#[tokio::test]
async fn a_childs_initial_action_reads_the_parent_after_the_spawning_action() {
    let state = build_state();
    let (status, body) = send(&state, Method::POST, "/tdata/Runs", json!({"Id": "r1"})).await;
    assert_eq!(status, StatusCode::CREATED, "seed r1: {body}");

    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Runs('r1')/Temper.Spawn.Launch",
        json!({"child_id": "c1"}),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        stored_run(&state, "r1").await,
        ("Running".to_string(), json!("c1"))
    );
}

#[path = "stored_record_spawn/ordered.rs"]
mod ordered;

#[path = "stored_record_spawn/contracts.rs"]
mod contracts;

#[path = "stored_record_spawn/authenticated.rs"]
mod authenticated;

/// A child made before its spec declared `grade` stores none. The
/// cross-invariant that reads it reads the declared initial value, as a
/// guard does.
#[tokio::test]
async fn a_cross_invariant_reads_a_related_records_newly_declared_field_as_its_default() {
    let state = build_state();
    let (status, body) = send(&state, Method::POST, "/tdata/Children", json!({"Id": "c1"})).await;
    assert_eq!(status, StatusCode::CREATED, "seed c1: {body}");

    let child =
        format!("{CHILD_IOA}\n[[state]]\nname = \"grade\"\ntype = \"string\"\ninitial = \"A\"\n");
    let graded = r#"
version = 1

[[invariant]]
name = "RunsNameAGradeAChild"
kind = "hard"
on = "Run.Create"
assert = 'related(Child, ChildRef).grade in ["A"]'
"#;
    {
        let mut registry = state.registry.write().unwrap();
        registry.register_tenant_with_constraints(
            "default",
            parse_csdl(CSDL).expect("CSDL parse"),
            CSDL.to_string(),
            &[("Child", child.as_str()), ("Run", RUN_IOA)],
            Some(graded.to_string()),
        );
        for entity_type in ["Child", "Run"] {
            registry.set_verification_status(&TenantId::default(), entity_type, verified());
        }
    }

    let (status, body) = send(
        &state,
        Method::POST,
        "/tdata/Runs",
        json!({"Id": "r1", "ChildRef": "c1"}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "c1 stores no grade, which reads as 'A': {body}"
    );
}
