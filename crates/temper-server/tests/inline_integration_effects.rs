//! Regression test for ARN-188.
//!
//! When `await_integration = true` and a transition emits BOTH a custom effect
//! (mapped to an inline integration that yields a callback response) AND a
//! `schedule` / `spawn` effect, the inline integration path must still run the
//! ORIGINAL transition's scheduled actions and spawn requests. The bug was an
//! early `return final_response` that skipped those steps, silently dropping the
//! scheduled timer and the child-entity spawn.

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_spec::csdl::parse_csdl;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.InlineEffects" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Parent">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityType Name="Child">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Parents" EntityType="Temper.InlineEffects.Parent"/>
        <EntitySet Name="Children" EntityType="Temper.InlineEffects.Child"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

// The `Trigger` transition emits, in one shot:
//   * a `trigger` custom effect  -> inline `http` adapter -> `AdapterSucceeded` callback
//   * a `schedule` effect        -> fires `Tick` on the parent (Done -> Ticked)
//   * a `spawn` effect           -> creates a `Child` and runs `Init` (New -> Initialized)
//
// Under `await_integration = true`, the inline adapter callback moves the parent
// to `Done`. The scheduled `Tick` and the spawned `Child` belong to the ORIGINAL
// `Trigger` response and must still be applied.
const PARENT_SPEC: &str = r#"
[automaton]
name = "Parent"
states = ["Idle", "Pending", "Done", "Ticked", "Failed"]
initial = "Idle"

[[action]]
name = "Trigger"
kind = "input"
from = ["Idle"]
to = "Pending"
params = ["child_id"]
effect = [
    { type = "trigger", name = "adapter_call" },
    { type = "schedule", action = "Tick", delay_seconds = 0 },
    { type = "spawn", entity_type = "Child", entity_id_source = "child_id", initial_action = "Init" },
]

[[action]]
name = "AdapterSucceeded"
kind = "input"
from = ["Pending"]
to = "Done"
params = ["result"]

[[action]]
name = "AdapterFailed"
kind = "input"
from = ["Pending"]
to = "Failed"
params = ["error_message"]

[[action]]
name = "Tick"
kind = "input"
from = ["Done"]
to = "Ticked"

[[integration]]
name = "adapter_call"
trigger = "adapter_call"
type = "adapter"
adapter = "http"
on_success = "AdapterSucceeded"
on_failure = "AdapterFailed"
url = "{url}/execute"
method = "POST"
"#;

const CHILD_SPEC: &str = r#"
[automaton]
name = "Child"
states = ["New", "Initialized"]
initial = "New"

[[action]]
name = "Init"
kind = "input"
from = ["New"]
to = "Initialized"
"#;

fn build_state(parent_spec: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Parent", parent_spec), ("Child", CHILD_SPEC)],
    );

    let system = ActorSystem::new("inline-integration-effects-test");
    ServerState::from_registry(system, registry)
}

/// Poll `get_tenant_entity_state` until the entity reaches `expected` status,
/// or the attempt budget is exhausted. Returns the last observed status.
async fn poll_status(
    state: &ServerState,
    tenant: &TenantId,
    entity_type: &str,
    entity_id: &str,
    expected: &str,
) -> String {
    let mut last = String::new();
    for _ in 0..100 {
        if let Ok(resp) = state
            .get_tenant_entity_state(tenant, entity_type, entity_id)
            .await
        {
            last = resp.state.status.clone();
            if last == expected {
                return last;
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    last
}

#[tokio::test(flavor = "multi_thread")]
async fn inline_integration_still_runs_scheduled_and_spawn_effects() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/execute"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "callback_params": { "result": "ok" }
        })))
        .mount(&mock_server)
        .await;

    let spec = PARENT_SPEC.replace("{url}", &mock_server.uri());
    let state = build_state(&spec);
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::default();

    let response = state
        .dispatch_tenant_action_ext(
            &tenant,
            "Parent",
            "parent-1",
            "Trigger",
            serde_json::json!({ "child_id": "child-1" }),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect("Trigger should succeed");

    // The inline adapter callback must have been applied.
    assert!(response.success);
    assert_eq!(
        response.state.status, "Done",
        "inline adapter callback should move parent to Done"
    );

    // The scheduled action from the ORIGINAL transition must still fire.
    let parent_status = poll_status(&state, &tenant, "Parent", "parent-1", "Ticked").await;
    assert_eq!(
        parent_status, "Ticked",
        "scheduled action `Tick` was dropped by the inline integration path (ARN-188)"
    );

    // The spawn request from the ORIGINAL transition must still create + init the child.
    let child_status = poll_status(&state, &tenant, "Child", "child-1", "Initialized").await;
    assert_eq!(
        child_status, "Initialized",
        "spawn request for child entity was dropped by the inline integration path (ARN-188)"
    );
}
