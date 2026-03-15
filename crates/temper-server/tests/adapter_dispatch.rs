//! Native adapter dispatch integration tests.

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_spec::csdl::parse_csdl;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const ADAPTER_CSDL_XML: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.AdapterTest" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="AdapterTest">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
        <Property Name="Status" Type="Edm.String"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="AdapterTests" EntityType="Temper.AdapterTest.AdapterTest"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

fn build_state(spec: &str) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(ADAPTER_CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        ADAPTER_CSDL_XML.to_string(),
        &[("AdapterTest", spec)],
    );

    let system = ActorSystem::new("adapter-dispatch-test");
    ServerState::from_registry(system, registry)
}

#[tokio::test(flavor = "multi_thread")]
async fn adapter_integration_dispatches_success_callback_inline() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/execute"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "callback_params": {
                "result": "ok"
            }
        })))
        .mount(&mock_server)
        .await;

    let spec = format!(
        r#"
[automaton]
name = "AdapterTest"
states = ["Idle", "Pending", "Done", "Failed"]
initial = "Idle"

[[action]]
name = "Trigger"
kind = "input"
from = ["Idle"]
to = "Pending"
effect = [{{ type = "trigger", name = "adapter_call" }}]

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

[[integration]]
name = "adapter_call"
trigger = "adapter_call"
type = "adapter"
adapter = "http"
on_success = "AdapterSucceeded"
on_failure = "AdapterFailed"
url = "{url}/execute"
method = "POST"
"#,
        url = mock_server.uri()
    );

    let state = build_state(&spec);
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::default();

    let response = state
        .dispatch_tenant_action_ext(
            &tenant,
            "AdapterTest",
            "adapter-1",
            "Trigger",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
            },
        )
        .await
        .expect("Trigger should succeed");

    assert!(response.success);
    assert_eq!(response.state.status, "Done");
}

#[tokio::test(flavor = "multi_thread")]
async fn adapter_integration_dispatches_failure_callback_inline() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/execute"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&mock_server)
        .await;

    let spec = format!(
        r#"
[automaton]
name = "AdapterTest"
states = ["Idle", "Pending", "Done", "Failed"]
initial = "Idle"

[[action]]
name = "Trigger"
kind = "input"
from = ["Idle"]
to = "Pending"
effect = [{{ type = "trigger", name = "adapter_call" }}]

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

[[integration]]
name = "adapter_call"
trigger = "adapter_call"
type = "adapter"
adapter = "http"
on_success = "AdapterSucceeded"
on_failure = "AdapterFailed"
url = "{url}/execute"
method = "POST"
"#,
        url = mock_server.uri()
    );

    let state = build_state(&spec);
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::default();

    let response = state
        .dispatch_tenant_action_ext(
            &tenant,
            "AdapterTest",
            "adapter-2",
            "Trigger",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
            },
        )
        .await
        .expect("Trigger should dispatch failure callback");

    assert!(response.success);
    assert_eq!(response.state.status, "Failed");
}

#[tokio::test(flavor = "multi_thread")]
async fn adapter_integration_uses_entity_adapter_type_over_static_config() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/execute"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "callback_params": {
                "result": "ok"
            }
        })))
        .mount(&mock_server)
        .await;

    let spec = format!(
        r#"
[automaton]
name = "AdapterTest"
states = ["Idle", "Pending", "Done", "Failed"]
initial = "Idle"

[[state]]
name = "adapter_type"
type = "string"
initial = "http"

[[action]]
name = "Configure"
kind = "input"
from = ["Idle"]
params = ["adapter_type"]

[[action]]
name = "Trigger"
kind = "input"
from = ["Idle"]
to = "Pending"
effect = [{{ type = "trigger", name = "adapter_call" }}]

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

[[integration]]
name = "adapter_call"
trigger = "adapter_call"
type = "adapter"
adapter = "claude_code"
on_success = "AdapterSucceeded"
on_failure = "AdapterFailed"
url = "{url}/execute"
method = "POST"
"#,
        url = mock_server.uri()
    );

    let state = build_state(&spec);
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::default();

    let configure = state
        .dispatch_tenant_action_ext(
            &tenant,
            "AdapterTest",
            "adapter-3",
            "Configure",
            serde_json::json!({ "adapter_type": "http" }),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
            },
        )
        .await
        .expect("Configure should set adapter_type");
    assert!(configure.success);

    let response = state
        .dispatch_tenant_action_ext(
            &tenant,
            "AdapterTest",
            "adapter-3",
            "Trigger",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
            },
        )
        .await
        .expect("Trigger should use adapter_type from entity state");

    assert!(response.success);
    assert_eq!(response.state.status, "Done");
}
