//! Regression coverage for bounded nested WASM callbacks.

use std::time::Duration;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_spec::csdl::parse_csdl;

const CALLBACK_CYCLE_IOA: &str = r#"
[automaton]
name = "CallbackCycle"
states = ["Looping", "Failed"]
initial = "Looping"
allow_indefinite_states = ["Looping"]

[[state]]
name = "callbacks"
type = "counter"
initial = "0"

[[action]]
name = "Start"
kind = "input"
from = ["Looping"]
to = "Looping"
effect = [{ type = "trigger", name = "missing_call" }]

[[action]]
name = "Again"
kind = "input"
from = ["Looping"]
to = "Looping"
guard = "callbacks < 20"
effect = [
  { type = "increment", var = "callbacks" },
  { type = "trigger", name = "missing_call" }
]

[[action]]
name = "Fail"
kind = "input"
from = ["Looping"]
to = "Failed"

[[integration]]
name = "missing_integration"
trigger = "missing_call"
type = "wasm"
module = "missing_integration"
on_failure = "Again"
"#;

const CALLBACK_CHAIN_IOA: &str = r#"
[automaton]
name = "CallbackChain"
states = ["Created", "One", "Two", "Three", "Complete", "Failed"]
initial = "Created"

[[action]]
name = "Start"
kind = "input"
from = ["Created"]
to = "One"
effect = [{ type = "trigger", name = "missing_one" }]

[[action]]
name = "ContinueOne"
kind = "input"
from = ["One"]
to = "Two"
effect = [{ type = "trigger", name = "missing_two" }]

[[action]]
name = "ContinueTwo"
kind = "input"
from = ["Two"]
to = "Three"
effect = [{ type = "trigger", name = "missing_three" }]

[[action]]
name = "Finish"
kind = "input"
from = ["Three"]
to = "Complete"

[[action]]
name = "Fail"
kind = "input"
from = ["One", "Two", "Three"]
to = "Failed"

[[integration]]
name = "missing_one_integration"
trigger = "missing_one"
type = "wasm"
module = "missing_one_integration"
on_failure = "ContinueOne"

[[integration]]
name = "missing_two_integration"
trigger = "missing_two"
type = "wasm"
module = "missing_two_integration"
on_failure = "ContinueTwo"

[[integration]]
name = "missing_three_integration"
trigger = "missing_three"
type = "wasm"
module = "missing_three_integration"
on_failure = "Finish"
"#;

const CALLBACK_CYCLE_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.CallbackCycle" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="CallbackCycle">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="CallbackCycles" EntityType="Temper.CallbackCycle.CallbackCycle"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

const CALLBACK_CHAIN_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.CallbackChain" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="CallbackChain">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="CallbackChains" EntityType="Temper.CallbackChain.CallbackChain"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

fn callback_cycle_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CALLBACK_CYCLE_CSDL).expect("callback-cycle CSDL should parse"),
        CALLBACK_CYCLE_CSDL.to_string(),
        &[("CallbackCycle", CALLBACK_CYCLE_IOA)],
    );
    ServerState::from_registry(ActorSystem::new("wasm-callback-budget"), registry)
}

fn callback_chain_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        parse_csdl(CALLBACK_CHAIN_CSDL).expect("callback-chain CSDL should parse"),
        CALLBACK_CHAIN_CSDL.to_string(),
        &[("CallbackChain", CALLBACK_CHAIN_IOA)],
    );
    ServerState::from_registry(ActorSystem::new("wasm-callback-chain"), registry)
}

async fn wait_for_status(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    terminal_statuses: &[&str],
) -> temper_server::entity_actor::EntityState {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let response = state
                .get_tenant_entity_state(tenant, entity_type, entity_id)
                .await
                .expect("callback entity should exist");
            if terminal_statuses.contains(&response.state.status.as_str()) {
                return response.state;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("background callback chain should reach a terminal state")
}

#[tokio::test(flavor = "multi_thread")]
async fn cyclic_inline_callbacks_exhaust_a_propagated_budget() {
    let state = callback_cycle_state();
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::default();

    let response = tokio::time::timeout(
        Duration::from_secs(5),
        state.dispatch_tenant_action_ext(
            &tenant,
            "CallbackCycle",
            "cycle-1",
            "Start",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
                await_reactions: true,
            },
        ),
    )
    .await
    .expect("a callback cycle must terminate within its explicit budget")
    .expect("the durable transition returns a surfaced integration result");

    assert!(!response.success, "budget exhaustion is a terminal error");
    let error = response.error.expect("budget exhaustion is surfaced");
    assert!(
        error.contains("WASM callback budget exhausted"),
        "unexpected terminal error: {error}"
    );
    assert_eq!(
        response.state.counters.get("callbacks"),
        Some(&2),
        "the callback budget permits exactly two nested transitions"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn acyclic_background_callbacks_preserve_valid_workflow_depth() {
    let state = callback_chain_state();
    let tenant = TenantId::default();

    let response = state
        .dispatch_tenant_action(
            &tenant,
            "CallbackChain",
            "chain-1",
            "Start",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("the first durable transition should dispatch");
    assert!(response.success);
    assert_eq!(response.state.status, "One");

    let final_state = wait_for_status(
        &state,
        &tenant,
        "CallbackChain",
        "chain-1",
        &["Complete", "Failed"],
    )
    .await;
    assert_eq!(
        final_state.status, "Complete",
        "a valid three-callback background workflow must not exhaust the cycle budget"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn cyclic_background_callbacks_exhaust_the_propagated_logical_budget() {
    let state = callback_cycle_state();
    let tenant = TenantId::default();

    state
        .dispatch_tenant_action(
            &tenant,
            "CallbackCycle",
            "cycle-background-1",
            "Start",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("the first durable transition should dispatch");

    let final_state = wait_for_status(
        &state,
        &tenant,
        "CallbackCycle",
        "cycle-background-1",
        &["Failed"],
    )
    .await;
    let expected_callbacks = usize::try_from(temper_spec::automaton::MAX_TRIGGER_DEPTH)
        .expect("the trigger-depth budget fits in usize");
    assert_eq!(
        final_state.counters.get("callbacks"),
        Some(&expected_callbacks),
        "background cycles terminate at the propagated logical callback budget"
    );
}
