//! A WASM integration is told who the move that set it off was made as.
//!
//! The integration's context carries `trigger_principal`: the principal Cedar
//! saw for the triggering move. A direct move's is its mover. A timer's move
//! is made as whoever set the timer, and a reaction as the mover of the move
//! it reacts to, so their integrations see that setter or mover. A move the
//! kernel makes itself carries none.
//!
//! The module answers with what it was given, as JSON in its callback's
//! `seen` parameter, which lands in the record's fields.

use std::time::{Duration, Instant};

use serde_json::{Value, json};
use temper_authz::{PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::{ServerState, StorageStack};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;

/// Answers `{"seen": <its context's trigger_principal as JSON>}`.
const ECHO_WASM: &[u8] =
    include_bytes!("../../../crates/temper-wasm/tests/fixtures/trigger_principal_echo.wasm");

/// `Run` triggers the module directly. `Park` arms a timer whose move, `Wake`,
/// triggers it. `Kick` sets off a reaction, `React`, which triggers it.
const JOB_IOA: &str = r#"
[automaton]
name = "Job"
states = ["Idle", "Parked", "Kicked", "Running", "Seen", "Failed"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Kicked", "Running", "Seen", "Failed"]

[[action]]
name = "Run"
kind = "input"
from = ["Idle"]
to = "Running"

[[action.triggers]]
name = "echo_run"
kind = "wasm"
module = "trigger_principal_echo"
on_success = "Saw"
on_failure = "Fail"

[[action]]
name = "Park"
kind = "input"
from = ["Idle"]
to = "Parked"

[[state_timeout]]
state = "Parked"
after_seconds = 1
on_timeout = "Wake"

[[action]]
name = "Wake"
kind = "internal"
from = ["Parked"]
to = "Running"

[[action.triggers]]
name = "echo_wake"
kind = "wasm"
module = "trigger_principal_echo"
on_success = "Saw"
on_failure = "Fail"

[[action]]
name = "Kick"
kind = "input"
from = ["Idle"]
to = "Kicked"

[[action.triggers]]
name = "reacts"
kind = "entity"
target_entity = "Job"
target_action = "React"
resolve_target = { kind = "same_id" }

[[action]]
name = "React"
kind = "input"
from = ["Kicked"]
to = "Running"

[[action.triggers]]
name = "echo_react"
kind = "wasm"
module = "trigger_principal_echo"
on_success = "Saw"
on_failure = "Fail"

[[action]]
name = "Saw"
kind = "input"
from = ["Running"]
to = "Seen"

[[action]]
name = "Fail"
kind = "input"
from = ["Running"]
to = "Failed"
"#;

const CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Trigger" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Job">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="seen" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Jobs" EntityType="Temper.Trigger.Job"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

fn server(name: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CSDL).expect("CSDL should parse"),
        CSDL.to_string(),
        &[("Job", JOB_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(name), registry);
    state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(7), None));
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .expect("policy should load");
    state.rebuild_reaction_dispatcher();
    let hash = state
        .wasm_engine
        .compile_and_cache(ECHO_WASM)
        .expect("the module compiles");
    state
        .wasm_module_registry
        .write()
        .expect("wasm registry lock") // ci-ok: infallible lock
        .register(&TenantId::default(), "trigger_principal_echo", &hash);
    state
}

fn as_who(security_ctx: SecurityContext) -> AgentContext {
    AgentContext {
        security_ctx: Some(security_ctx),
        ..AgentContext::default()
    }
}

fn person(id: &str) -> AgentContext {
    as_who(SecurityContext::from_verified_jwt(
        id,
        PrincipalKind::Customer,
        None,
        None,
        None,
        None,
    ))
}

fn agent_for(person: &str) -> AgentContext {
    as_who(SecurityContext::from_verified_jwt(
        "agent-1",
        PrincipalKind::Agent,
        Some("contributor"),
        Some(person),
        None,
        None,
    ))
}

/// Create job `id` and make `action` on it as `by`.
async fn act(state: &ServerState, id: &str, action: &str, by: &AgentContext) {
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(&tenant, "Job", id, json!({}))
        .await
        .expect("the job is created");
    let response = state
        .dispatch_tenant_action(&tenant, "Job", id, action, json!({}), by)
        .await
        .expect("the move is dispatched");
    assert!(response.success, "{action} on {id}: {:?}", response.error);
}

/// What job `id`'s integration saw as its trigger principal, once its
/// callback landed.
async fn seen(state: &ServerState, id: &str) -> Value {
    let tenant = TenantId::default();
    let deadline = Instant::now() + Duration::from_secs(30); // determinism-ok: real-time test
    loop {
        let job = state
            .get_tenant_entity_state(&tenant, "Job", id)
            .await
            .expect("the job exists")
            .state;
        if job.status == "Seen" {
            let seen = job.fields.get("seen").and_then(Value::as_str).unwrap_or("");
            return serde_json::from_str(seen).expect("the module answers JSON");
        }
        assert!(
            job.status != "Failed" && Instant::now() < deadline, // determinism-ok: real-time test
            "job {id} never saw its trigger: {} {:?}",
            job.status,
            job.fields
        );
        tokio::time::sleep(Duration::from_millis(50)).await; // determinism-ok: real-time test
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_moves_integration_sees_its_mover() {
    let state = server("trigger-principal-direct");
    act(&state, "j-direct", "Run", &person("person-1")).await;
    assert_eq!(
        seen(&state, "j-direct").await,
        json!({ "id": "person-1", "kind": "Customer", "acting_for": null, "agent_type": null })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_timer_fired_integration_sees_the_timers_setter() {
    let state = server("trigger-principal-timer");
    act(&state, "j-timer", "Park", &agent_for("person-2")).await;
    assert_eq!(
        seen(&state, "j-timer").await,
        json!({
            "id": "agent-1",
            "kind": "Agent",
            "acting_for": "person-2",
            "agent_type": "contributor"
        })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reaction_triggered_integration_sees_the_original_mover() {
    let state = server("trigger-principal-reaction");
    act(&state, "j-reaction", "Kick", &person("person-3")).await;
    assert_eq!(
        seen(&state, "j-reaction").await,
        json!({ "id": "person-3", "kind": "Customer", "acting_for": null, "agent_type": null })
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_system_moves_integration_sees_no_principal() {
    let state = server("trigger-principal-system");
    // The same server tells a person's integration who they are, so the
    // system move's empty answer is the kernel leaving itself out.
    act(&state, "j-person", "Run", &person("person-4")).await;
    act(&state, "j-system", "Run", &AgentContext::system()).await;
    assert_eq!(seen(&state, "j-person").await["id"], "person-4");
    assert_eq!(seen(&state, "j-system").await, Value::Null);
}
