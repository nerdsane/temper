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
                await_reactions: true,
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
                await_reactions: true,
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
                await_reactions: true,
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
                await_reactions: true,
            },
        )
        .await
        .expect("Trigger should use adapter_type from entity state");

    assert!(response.success);
    assert_eq!(response.state.status, "Done");
}

// ---------------------------------------------------------------------------
// ADR-0152: integration failure is never silent.
// ---------------------------------------------------------------------------

/// Spec WITHOUT `on_failure`, but WITH a `Fail` transition for compensation.
fn no_on_failure_spec_with_fail(mock_uri: &str) -> String {
    format!(
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
name = "Fail"
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
url = "{mock_uri}/execute"
method = "POST"
"#
    )
}

/// Spec WITHOUT `on_failure` and WITHOUT any failure transition.
fn no_on_failure_spec_no_fail(mock_uri: &str) -> String {
    format!(
        r#"
[automaton]
name = "AdapterTest"
states = ["Idle", "Pending", "Done"]
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

[[integration]]
name = "adapter_call"
trigger = "adapter_call"
type = "adapter"
adapter = "http"
on_success = "AdapterSucceeded"
url = "{mock_uri}/execute"
method = "POST"
"#
    )
}

async fn failing_mock() -> MockServer {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/execute"))
        .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
        .mount(&mock_server)
        .await;
    mock_server
}

async fn poll_status(
    state: &ServerState,
    tenant: &TenantId,
    entity_id: &str,
    want: &str,
) -> String {
    for _ in 0..100 {
        let status = state
            .get_tenant_entity_state(tenant, "AdapterTest", entity_id)
            .await
            .map(|r| r.state.status)
            .unwrap_or_default();
        if status == want {
            return status;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    state
        .get_tenant_entity_state(tenant, "AdapterTest", entity_id)
        .await
        .map(|r| r.state.status)
        .unwrap_or_default()
}

/// Inline, no `on_failure`: the failed integration must surface as
/// `success: false` to the caller, not masquerade as success (ADR-0152).
#[tokio::test(flavor = "multi_thread")]
async fn inline_failure_without_on_failure_returns_success_false() {
    let mock_server = failing_mock().await;
    let spec = no_on_failure_spec_with_fail(&mock_server.uri());
    let state = build_state(&spec);
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::default();

    let response = state
        .dispatch_tenant_action_ext(
            &tenant,
            "AdapterTest",
            "adapter-inline-nofail",
            "Trigger",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect("dispatch returns a response");

    assert!(
        !response.success,
        "an inline integration failure with no on_failure must report success: false, got {response:?}"
    );
    assert!(
        response
            .error
            .as_deref()
            .is_some_and(|e| e.contains("integration")),
        "error should name the integration failure, got {:?}",
        response.error
    );
}

/// Background, no `on_failure`, WITH a `Fail` transition: the durable
/// transition is compensated forward — the entity reaches `Failed` (ADR-0152).
#[tokio::test(flavor = "multi_thread")]
async fn background_failure_without_on_failure_dispatches_compensating_fail() {
    let mock_server = failing_mock().await;
    let spec = no_on_failure_spec_with_fail(&mock_server.uri());
    let state = build_state(&spec);
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::default();

    let response = state
        .dispatch_tenant_action_ext(
            &tenant,
            "AdapterTest",
            "adapter-bg-fail",
            "Trigger",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: false,
                await_reactions: false,
            },
        )
        .await
        .expect("background Trigger commits its transition");

    // The source transition is durable, so Trigger itself succeeds.
    assert!(response.success);
    assert_eq!(response.state.status, "Pending");

    // Compensation drives the entity to its declared Fail state.
    let status = poll_status(&state, &tenant, "adapter-bg-fail", "Failed").await;
    assert_eq!(
        status, "Failed",
        "background integration failure must compensate via the Fail transition"
    );
}

/// Background, no `on_failure`, NO failure transition: the failure cannot be
/// compensated, so the entity must NOT be driven to success — it stays in
/// `Pending` and the drop is surfaced (metric + Observe event), never silent.
#[tokio::test(flavor = "multi_thread")]
async fn background_failure_without_fail_transition_does_not_reach_success() {
    let mock_server = failing_mock().await;
    let spec = no_on_failure_spec_no_fail(&mock_server.uri());
    let state = build_state(&spec);
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::default();

    state
        .dispatch_tenant_action_ext(
            &tenant,
            "AdapterTest",
            "adapter-bg-nofailpath",
            "Trigger",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: false,
                await_reactions: false,
            },
        )
        .await
        .expect("background Trigger commits its transition");

    // Give the background compensation a chance to run, then confirm the
    // entity did NOT advance to Done (the success state) — the failure was not
    // swallowed as success.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let status = state
        .get_tenant_entity_state(&tenant, "AdapterTest", "adapter-bg-nofailpath")
        .await
        .map(|r| r.state.status)
        .unwrap_or_default();
    assert_eq!(
        status, "Pending",
        "with no failure path, the entity stays in Pending — it must never reach Done on failure"
    );
}
