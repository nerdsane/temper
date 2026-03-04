//! WASM dispatch → callback end-to-end integration test.
//!
//! Exercises the full ServerState chain:
//! action → custom_effects → dispatch_wasm_integrations → WasmEngine.invoke()
//! → callback dispatched → entity state transitions.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerEventStore;
use temper_server::ServerState;
use temper_server::dispatch::AgentContext;
use temper_server::registry::SpecRegistry;
use temper_server::state::{DispatchExtOptions, PendingDecision};
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

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

/// Build a test state with a local Turso (SQLite) backend so that
/// persisted artifacts (decisions, trajectories, invocations) can be
/// queried after dispatch.
async fn build_echo_test_state_with_turso() -> ServerState {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after UNIX epoch")
        .as_nanos();
    let db_url = format!("file:/tmp/temper-wasm-dispatch-test-{}-{ts}.db", std::process::id());
    // Clean up any leftover DB + WAL/SHM files from a previous run.
    let db_path = db_url.strip_prefix("file:").unwrap_or(&db_url);
    let _ = std::fs::remove_file(db_path);
    let _ = std::fs::remove_file(format!("{db_path}-wal"));
    let _ = std::fs::remove_file(format!("{db_path}-shm"));
    let turso = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = build_echo_test_state();
    state.event_store = Some(Arc::new(ServerEventStore::Turso(turso)));
    state
}

async fn wait_for_status(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    terminal_statuses: &[&str],
    timeout: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let entity = state
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
            .expect("entity should exist");
        let status = entity.state.status.clone();
        if terminal_statuses.contains(&status.as_str()) || tokio::time::Instant::now() >= deadline
        {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

const ADMIN_ONLY_POLICY: &str = r#"
permit(
  principal is Admin,
  action == Action::"manage_policies",
  resource is PolicySet
);
"#;

fn install_non_wasm_policy(state: &ServerState) {
    state
        .authz
        .reload_policies(ADMIN_ONLY_POLICY)
        .expect("policy should parse");
}

/// Verify authz denial artifacts are persisted to Turso.
///
/// Checks that the WASM authorization denial pathway creates:
/// 1. A PendingDecision for the denied http_call action
/// 2. A trajectory entry with authz_denied flag and source=Authz
/// 3. A WASM invocation entry recording the failed invocation
async fn assert_wasm_authz_denial_artifacts(state: &ServerState, entity_id: &str) {
    let turso = state
        .turso_opt()
        .expect("Turso backend required for authz denial artifact verification");

    // 1. Verify PendingDecision was persisted.
    let mut decision = None;
    for _ in 0..100 {
        let decisions = turso
            .query_all_decisions(None)
            .await
            .expect("query decisions from Turso");
        decision = decisions
            .iter()
            .filter_map(|data| serde_json::from_str::<PendingDecision>(data).ok())
            .find(|d| d.resource_id == "echo_integration" && d.action == "http_call");
        if decision.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let decision = decision
        .expect("expected wasm authz pending decision in Turso");
    assert_eq!(decision.module_name.as_deref(), Some("echo_integration"));

    // 2. Verify authz trajectory entry was persisted.
    let mut authz_traj = None;
    for _ in 0..100 {
        let trajectories = turso
            .load_recent_trajectories(1000)
            .await
            .expect("query trajectories from Turso");
        authz_traj = trajectories
            .iter()
            .find(|t| {
                t.entity_id == entity_id
                    && t.authz_denied == Some(true)
                    && t.source.as_deref() == Some("Authz")
            })
            .cloned();
        if authz_traj.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let authz_traj = authz_traj
        .expect("expected authz trajectory in Turso");
    assert_eq!(
        authz_traj.denied_module.as_deref(),
        Some("echo_integration")
    );

    // 3. Verify denied WASM invocation was persisted.
    // The wasm_invocation_logs table does not store authz_denied directly;
    // we identify the denied invocation by entity_id + failure + error text.
    let mut denied_invocation = None;
    for _ in 0..100 {
        let invocations = turso
            .load_recent_wasm_invocations(1000)
            .await
            .expect("query wasm invocations from Turso");
        denied_invocation = invocations
            .iter()
            .find(|w| {
                w.entity_id == entity_id
                    && !w.success
                    && w.error
                        .as_deref()
                        .is_some_and(|e| e.contains("authorization denied"))
            })
            .cloned();
        if denied_invocation.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let denied_invocation = denied_invocation
        .expect("expected denied wasm invocation in Turso");
    assert_eq!(denied_invocation.module_name, "echo_integration");
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
    let final_status = wait_for_status(
        &state,
        &tenant,
        "EchoTest",
        "echo-1",
        &["Done", "Failed"],
        Duration::from_secs(20),
    )
    .await;

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
    let final_status = wait_for_status(
        &state,
        &tenant,
        "EchoTest",
        "echo-missing",
        &["Failed", "Done"],
        Duration::from_secs(20),
    )
    .await;

    assert_eq!(
        final_status, "Failed",
        "missing module should trigger on_failure callback → Failed state"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_authz_denial_records_governance_artifacts_async_mode() {
    let state = build_echo_test_state_with_turso().await;
    let tenant = TenantId::default();
    install_non_wasm_policy(&state);

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

    let response = state
        .dispatch_tenant_action(
            &tenant,
            "EchoTest",
            "echo-authz-async",
            "TriggerEcho",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("TriggerEcho should succeed");
    assert_eq!(response.state.status, "Pending");

    let final_status = wait_for_status(
        &state,
        &tenant,
        "EchoTest",
        "echo-authz-async",
        &["Failed"],
        Duration::from_secs(20),
    )
    .await;
    assert_eq!(final_status, "Failed");
    assert_wasm_authz_denial_artifacts(&state, "echo-authz-async").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wasm_authz_denial_records_governance_artifacts_blocking_mode() {
    let state = build_echo_test_state_with_turso().await;
    let tenant = TenantId::default();
    install_non_wasm_policy(&state);

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

    let agent_ctx = AgentContext::default();
    let response = state
        .dispatch_tenant_action_ext(
            &tenant,
            "EchoTest",
            "echo-authz-blocking",
            "TriggerEcho",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
            },
        )
        .await
        .expect("blocking TriggerEcho should return callback result");
    assert!(response.success);
    assert_eq!(response.state.status, "Failed");
    assert_wasm_authz_denial_artifacts(&state, "echo-authz-blocking").await;
}
