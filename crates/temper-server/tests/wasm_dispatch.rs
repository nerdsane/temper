//! WASM dispatch → callback end-to-end integration test.
//!
//! Exercises the full ServerState chain:
//! action → custom_effects → dispatch_wasm_integrations → WasmEngine.invoke()
//! → callback dispatched → entity state transitions.

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::dispatch::AgentContext;
use temper_server::registry::SpecRegistry;
use temper_spec::csdl::parse_csdl;

/// Pre-built echo integration WASM binary.
const ECHO_WASM: &[u8] =
    include_bytes!("../../../crates/temper-wasm/tests/fixtures/echo_integration.wasm");

/// IOA spec with a `trigger echo_call` effect and WASM integration.
const ECHO_IOA: &str = r#"
[automaton]
name = "EchoTest"
states = ["Idle", "Pending", "Done", "Failed"]
initial = "Idle"

[[action]]
name = "TriggerEcho"
kind = "input"
from = ["Idle"]
to = "Pending"
effect = "trigger echo_call"
hint = "Kicks off the echo integration."

[[action]]
name = "EchoSucceeded"
kind = "input"
from = ["Pending"]
to = "Done"
hint = "Callback from successful echo WASM module."

[[action]]
name = "EchoFailed"
kind = "input"
from = ["Pending"]
to = "Failed"
hint = "Callback from failed echo WASM module."

[[integration]]
name = "echo_integration"
trigger = "echo_call"
type = "wasm"
module = "echo_integration"
on_success = "EchoSucceeded"
on_failure = "EchoFailed"
"#;

/// Minimal CSDL with EchoTest entity type.
const ECHO_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.EchoTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="EchoTest">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="EchoTests" EntityType="Temper.EchoTest.EchoTest"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

fn build_echo_test_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(ECHO_CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        ECHO_CSDL_XML.to_string(),
        &[("EchoTest", ECHO_IOA)],
    );

    let system = ActorSystem::new("wasm-dispatch-test");
    ServerState::from_registry(system, registry)
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_integration_dispatches_callback() {
    let state = build_echo_test_state();
    let tenant = TenantId::default();

    // Register the WASM module in the engine and module registry.
    let hash = state
        .wasm_engine
        .compile_and_cache(ECHO_WASM)
        .expect("echo module should compile");
    {
        let mut wasm_reg = state
            .wasm_module_registry
            .write()
            .expect("wasm registry lock"); // ci-ok: infallible lock
        wasm_reg.register(&tenant, "echo_integration", &hash);
    }

    // Dispatch TriggerEcho — should succeed and emit custom effect "echo_call".
    let response = state
        .dispatch_tenant_action(
            &tenant,
            "EchoTest",
            "echo-1",
            "TriggerEcho",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("TriggerEcho should succeed");

    assert!(response.success, "TriggerEcho should succeed");
    assert_eq!(response.state.status, "Pending");
    assert!(
        response.custom_effects.contains(&"echo_call".to_string()),
        "should emit echo_call effect, got: {:?}",
        response.custom_effects
    );

    // Poll for the callback to be dispatched asynchronously.
    // The WASM module is invoked in a tokio::spawn task, and its callback
    // (EchoSucceeded or EchoFailed) is dispatched back to the entity actor.
    let mut final_status = String::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let entity = state
            .get_tenant_entity_state(&tenant, "EchoTest", "echo-1")
            .await
            .expect("entity should exist");
        final_status = entity.state.status.clone();
        if final_status == "Done" || final_status == "Failed" {
            break;
        }
    }

    // The echo module calls https://echo.example.com/ping via ProductionWasmHost.
    // ProductionWasmHost makes a real HTTP call that will fail (DNS resolution).
    // The echo module handles HTTP failure gracefully: it returns "-1\n" as the
    // response and still reports success with callback_action = "EchoSucceeded".
    // So the on_success callback fires, transitioning to "Done".
    assert_eq!(
        final_status, "Done",
        "entity should transition to Done after WASM callback (echo module returns success even on HTTP failure)"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_missing_module_dispatches_failure_callback() {
    let state = build_echo_test_state();
    let tenant = TenantId::default();

    // Do NOT register any WASM module — the module registry is empty.
    // dispatch_wasm_integrations should detect the missing module and fire on_failure.

    let response = state
        .dispatch_tenant_action(
            &tenant,
            "EchoTest",
            "echo-missing",
            "TriggerEcho",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("TriggerEcho should succeed (transition is valid)");

    assert!(response.success);
    assert_eq!(response.state.status, "Pending");

    // Poll for the failure callback.
    let mut final_status = String::new();
    for _ in 0..50 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let entity = state
            .get_tenant_entity_state(&tenant, "EchoTest", "echo-missing")
            .await
            .expect("entity should exist");
        final_status = entity.state.status.clone();
        if final_status == "Failed" || final_status == "Done" {
            break;
        }
    }

    assert_eq!(
        final_status, "Failed",
        "missing module should trigger on_failure callback → Failed state"
    );
}
