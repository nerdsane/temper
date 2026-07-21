//! Regression coverage for bounded nested inline WASM callbacks.

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
states = ["Looping"]
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

[[integration]]
name = "missing_integration"
trigger = "missing_call"
type = "wasm"
module = "missing_integration"
on_failure = "Again"
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
        Some(&16),
        "the callback budget permits exactly sixteen nested transitions"
    );
}
