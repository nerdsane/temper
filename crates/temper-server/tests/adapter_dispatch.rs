//! Native adapter dispatch integration tests (ADR-0160 / ARN-228).

use std::sync::Once;

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_spec::csdl::parse_csdl;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

static ALLOW_LOOPBACK: Once = Once::new();

/// Mock servers bind loopback; opt in once for the test process (ARN-228).
fn allow_adapter_loopback_for_tests() {
    ALLOW_LOOPBACK.call_once(|| {
        // SAFETY: test process init; set once before concurrent tests start.
        unsafe {
            std::env::set_var("TEMPER_ADAPTER_ALLOW_HTTP_LOOPBACK", "1");
        }
    });
}

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
    allow_adapter_loopback_for_tests();
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
    allow_adapter_loopback_for_tests();
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
    allow_adapter_loopback_for_tests();
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

/// ARN-228: entity `adapter_type` must NOT override the declared integration.
/// Declared adapter is `claude_code` (removed from kernel). Even if the entity
/// field is set to `http`, dispatch must fail closed — not escalate to HTTP.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_entity_field_cannot_override_declared_adapter() {
    allow_adapter_loopback_for_tests();
    let mock_server = MockServer::start().await;
    // If the bug regresses and entity field wins, this mock would succeed the run.
    Mock::given(method("POST"))
        .and(path("/execute"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "callback_params": { "result": "should-not-run" }
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
        .expect("Trigger should fail closed on removed host adapter");

    assert!(response.success);
    assert_eq!(
        response.state.status, "Failed",
        "entity adapter_type=http must not override declared claude_code"
    );
}

/// Undeclared / removed process adapters fail closed (no host spawn).
#[tokio::test(flavor = "multi_thread")]
async fn adapter_undeclared_process_adapter_fails_closed() {
    allow_adapter_loopback_for_tests();
    let spec = r#"
[automaton]
name = "AdapterTest"
states = ["Idle", "Pending", "Done", "Failed"]
initial = "Idle"

[[action]]
name = "Trigger"
kind = "input"
from = ["Idle"]
to = "Pending"
effect = [{ type = "trigger", name = "adapter_call" }]

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
adapter = "codex"
on_success = "AdapterSucceeded"
on_failure = "AdapterFailed"
"#;

    let state = build_state(spec);
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::default();

    let response = state
        .dispatch_tenant_action_ext(
            &tenant,
            "AdapterTest",
            "adapter-codex-denied",
            "Trigger",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect("Trigger should complete with failure callback");

    assert!(response.success);
    assert_eq!(response.state.status, "Failed");
}

/// Private / metadata origins are rejected before the request is sent.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_http_blocks_private_origin() {
    allow_adapter_loopback_for_tests();
    let spec = r#"
[automaton]
name = "AdapterTest"
states = ["Idle", "Pending", "Done", "Failed"]
initial = "Idle"

[[action]]
name = "Trigger"
kind = "input"
from = ["Idle"]
to = "Pending"
effect = [{ type = "trigger", name = "adapter_call" }]

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
url = "https://169.254.169.254/latest/meta-data/"
method = "GET"
"#;

    let state = build_state(spec);
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::default();

    let response = state
        .dispatch_tenant_action_ext(
            &tenant,
            "AdapterTest",
            "adapter-ssrf",
            "Trigger",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect("Trigger should fail closed on metadata SSRF");

    assert!(response.success);
    assert_eq!(response.state.status, "Failed");
}

/// Full tenant secret map is not attached to adapter context / HTTP payload.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_http_payload_does_not_include_full_secret_map() {
    allow_adapter_loopback_for_tests();
    use temper_server::secrets::vault::SecretsVault;
    use wiremock::matchers::body_partial_json;

    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/execute"))
        // Reject if the leaked secret value appears anywhere in the body.
        .and(body_partial_json(serde_json::json!({
            "tenant": "default"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "callback_params": { "result": "ok" }
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

    let vault = SecretsVault::new(&[7u8; 32]);
    vault
        .cache_secret(
            "default",
            "LEAKED_TENANT_SECRET",
            "super-secret-value-xyz".to_string(),
        )
        .expect("cache secret");
    let state = build_state(&spec).with_secrets_vault(vault);
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::default();

    let response = state
        .dispatch_tenant_action_ext(
            &tenant,
            "AdapterTest",
            "adapter-secrets",
            "Trigger",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect("Trigger should succeed without secret map");

    assert!(response.success);
    assert_eq!(response.state.status, "Done");

    let requests = mock_server.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 1, "exactly one adapter HTTP call");
    let body = String::from_utf8_lossy(&requests[0].body);
    assert!(
        !body.contains("super-secret-value-xyz"),
        "tenant secret value must not appear in adapter payload: {body}"
    );
    assert!(
        !body.contains("LEAKED_TENANT_SECRET"),
        "secret key name must not appear as a dumped secrets map: {body}"
    );
    assert!(
        !body.contains("\"secrets\""),
        "payload must not include a secrets object: {body}"
    );
    assert!(
        !body.contains("agent_api_key"),
        "payload must not include ambient platform credential: {body}"
    );
}

/// Oversized HTTP responses fail closed without success callback.
#[tokio::test(flavor = "multi_thread")]
async fn adapter_http_oversized_response_fails_closed() {
    allow_adapter_loopback_for_tests();
    let mock_server = MockServer::start().await;
    let big = "x".repeat(temper_server::adapters::ADAPTER_MAX_RESPONSE_BYTES + 64);
    Mock::given(method("POST"))
        .and(path("/execute"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header(
                    "content-length",
                    (temper_server::adapters::ADAPTER_MAX_RESPONSE_BYTES + 64).to_string(),
                )
                .set_body_string(big),
        )
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
            "adapter-oversized",
            "Trigger",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &agent_ctx,
                await_integration: true,
                await_reactions: true,
            },
        )
        .await
        .expect("Trigger should complete with failure callback");
    assert!(response.success);
    assert_eq!(response.state.status, "Failed");
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
