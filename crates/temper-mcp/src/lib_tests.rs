use super::*;
use temper_sandbox::helpers::format_authz_denied;

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Router;
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_server::{ServerEventStore, ServerState};
use temper_spec::parse_csdl;
use temper_store_turso::TursoEventStore;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

/// Build a RuntimeContext pointing at a local port.
fn ctx_for_port(port: u16) -> RuntimeContext {
    RuntimeContext::from_config(&McpConfig {
        temper_port: Some(port),
        temper_url: None,
        principal_id: None,
        api_key: None,
    })
    .expect("ctx")
}

/// Build a RuntimeContext pointing at a URL.
fn ctx_for_url(url: &str) -> RuntimeContext {
    RuntimeContext::from_config(&McpConfig {
        temper_port: None,
        temper_url: Some(url.to_string()),
        principal_id: None,
        api_key: None,
    })
    .expect("ctx")
}

async fn rpc(ctx: &RuntimeContext, request: Value) -> Value {
    dispatch_json_value(ctx, request)
        .await
        .expect("response expected")
}

fn call_tool_request(id: i64, tool: &str, code: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/call",
        "params": {
            "name": tool,
            "arguments": {
                "code": code
            }
        }
    })
}

fn tool_text(response: &Value) -> (&str, bool) {
    let is_error = response
        .pointer("/result/isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let text = response
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or("");
    (text, is_error)
}

async fn start_test_temper_server() -> (u16, oneshot::Sender<()>) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let db_url = format!("file:/tmp/temper-mcp-test-{}-{}.db", std::process::id(), id,);
    let _ = std::fs::remove_file(db_url.strip_prefix("file:").unwrap_or(&db_url));
    let turso = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    let csdl = parse_csdl(csdl_xml).expect("parse csdl");

    let mut ioa_sources = BTreeMap::new();
    ioa_sources.insert(
        "Order".to_string(),
        include_str!("../../../test-fixtures/specs/order.ioa.toml").to_string(),
    );

    let mut state = ServerState::with_specs(
        ActorSystem::new("temper-mcp-tests"),
        csdl,
        csdl_xml.to_string(),
        ioa_sources,
    )
    .unwrap();
    state.event_store = Some(Arc::new(ServerEventStore::Turso(turso)));

    let router: Router = temper_server::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    tokio::spawn(async move {
        let server = axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await;
        if let Err(err) = server {
            panic!("test server failed: {err}");
        }
    });

    (port, shutdown_tx)
}

#[tokio::test]
async fn mcp_initialize_handshake() {
    let ctx = ctx_for_port(3001);

    let response = rpc(
        &ctx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05"
            }
        }),
    )
    .await;

    assert_eq!(response["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
    assert_eq!(response["result"]["serverInfo"]["name"], MCP_SERVER_NAME);
    assert!(response["result"]["capabilities"]["tools"].is_object());
}

#[tokio::test]
async fn tool_list_has_single_execute_tool() {
    let ctx = ctx_for_port(3001);

    let response = rpc(
        &ctx,
        json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list"
        }),
    )
    .await;

    let tools = response["result"]["tools"].as_array().expect("tools array");
    assert_eq!(tools.len(), 1, "should have exactly one tool");
    assert_eq!(tools[0]["name"], "execute");
}

#[test]
fn from_config_requires_port_or_url() {
    let result = RuntimeContext::from_config(&McpConfig {
        temper_port: None,
        temper_url: None,
        principal_id: None,
        api_key: None,
    });
    assert!(result.is_err(), "should fail without --port or --url");
    let err = result.err().expect("expected error").to_string();
    assert!(
        err.contains("--url") && err.contains("--port"),
        "error should mention both flags: {err}"
    );
}

#[test]
fn from_config_url_mode() {
    let ctx = ctx_for_url("https://api.temper.build/");
    assert_eq!(ctx.base_url, "https://api.temper.build");
}

#[test]
fn from_config_port_mode() {
    let ctx = ctx_for_port(4000);
    assert_eq!(ctx.base_url, "http://127.0.0.1:4000");
}

#[tokio::test]
async fn execute_url_mode_works() {
    let (port, shutdown) = start_test_temper_server().await;
    let ctx = ctx_for_url(&format!("http://127.0.0.1:{port}"));

    let response = rpc(
        &ctx,
        call_tool_request(
            3,
            "execute",
            "return await temper.create('demo', 'Orders', {'id': 'url-mode-1', 'customer': 'Alice'})",
        ),
    )
    .await;

    let _ = shutdown.send(());

    let (text, is_error) = tool_text(&response);
    assert!(
        !is_error,
        "execute via URL mode should succeed: {response:#}"
    );
    let parsed: Value = serde_json::from_str(text).expect("json");
    assert_eq!(parsed["fields"]["customer"], "Alice");
}

#[tokio::test]
async fn execute_creates_entity_and_reads_it_back() {
    let (port, shutdown) = start_test_temper_server().await;
    let ctx = ctx_for_port(port);

    let response = rpc(
        &ctx,
        call_tool_request(
            3,
            "execute",
            r#"
created = await temper.create('demo', 'Orders', {'id': 'mcp-create-1', 'customer': 'Alice'})
fetched = await temper.get('demo', 'Orders', 'mcp-create-1')
return fetched['fields']['customer']
"#,
        ),
    )
    .await;

    let _ = shutdown.send(());

    let (text, is_error) = tool_text(&response);
    assert!(!is_error, "execute should succeed: {response:#}");
    let parsed: Value = serde_json::from_str(text).expect("tool text should be json");
    assert_eq!(parsed, Value::String("Alice".to_string()));
}

#[tokio::test]
async fn execute_invalid_action_returns_409_cleanly() {
    let (port, shutdown) = start_test_temper_server().await;
    let ctx = ctx_for_port(port);

    let response = rpc(
            &ctx,
            call_tool_request(
                4,
                "execute",
                r#"
await temper.create('demo', 'Orders', {'id': 'mcp-bad-action-1'})
await temper.action('demo', 'Orders', 'mcp-bad-action-1', 'ShipOrder', {'Reason': 'invalid from draft'})
return 'unreachable'
"#,
            ),
        )
        .await;

    let _ = shutdown.send(());

    let (text, is_error) = tool_text(&response);
    assert!(is_error, "execute should return tool error: {response:#}");
    assert!(
        text.contains("409"),
        "expected 409 in error message: {text}"
    );
}

#[tokio::test]
async fn execute_supports_compound_operation() {
    let (port, shutdown) = start_test_temper_server().await;
    let ctx = ctx_for_port(port);

    let response = rpc(
        &ctx,
        call_tool_request(
            6,
            "execute",
            r#"
await temper.create('demo', 'Orders', {'id': 'mcp-compound-1'})
await temper.action('demo', 'Orders', 'mcp-compound-1', 'CancelOrder', {'Reason': 'cancel test'})
fetched = await temper.get('demo', 'Orders', 'mcp-compound-1')
return fetched['status']
"#,
        ),
    )
    .await;

    let _ = shutdown.send(());

    let (text, is_error) = tool_text(&response);
    assert!(!is_error, "compound execute should succeed: {response:#}");
    let parsed: Value = serde_json::from_str(text).expect("tool text should be json");
    assert_eq!(parsed, Value::String("Cancelled".to_string()));
}

#[tokio::test]
async fn execute_specs_returns_data() {
    let (port, shutdown) = start_test_temper_server().await;
    let ctx = ctx_for_port(port);

    // The test server uses with_specs() which doesn't populate the spec registry.
    // The /observe/specs endpoint reads from the registry, so it returns an empty list
    // (not a 404). This verifies the specs() method dispatches correctly.
    let response = rpc(
        &ctx,
        call_tool_request(10, "execute", "return await temper.specs('demo')"),
    )
    .await;

    let _ = shutdown.send(());

    let (text, is_error) = tool_text(&response);
    assert!(!is_error, "specs should succeed: {response:#}");
    let parsed: Value = serde_json::from_str(text).expect("json");
    assert!(
        parsed.is_object() || parsed.is_array(),
        "specs should return structured data: {parsed}"
    );
}

/// End-to-end governance flow: agent denied -> get decision -> human approves
/// directly (not via agent sandbox) -> agent retries -> success.
#[tokio::test]
#[ignore = "requires Cedar agent default-deny policy in test server setup"]
async fn e2e_agent_denial_human_approve_retry() {
    let (port, shutdown) = start_test_temper_server().await;

    // Use agent identity so Cedar authorization applies (default-deny for agents).
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(port),
        temper_url: None,
        principal_id: Some("checkout-bot".to_string()),
        api_key: None,
    })
    .expect("ctx");

    // Step 0: Create entity first (creation bypasses Cedar).
    let response = rpc(
        &ctx,
        call_tool_request(
            20,
            "execute",
            "return await temper.create('demo', 'Orders', {'id': 'agent-e2e-1', 'customer': 'Alice'})",
        ),
    )
    .await;
    let (_, is_error) = tool_text(&response);
    assert!(
        !is_error,
        "create should succeed (no Cedar on create): {response:#}"
    );

    // Step 1: Agent tries a bound action — should be denied (403).
    let response = rpc(
        &ctx,
        call_tool_request(
            21,
            "execute",
            "return await temper.action('demo', 'Orders', 'agent-e2e-1', 'CancelOrder', {'Reason': 'test'})",
        ),
    )
    .await;
    let (text, is_error) = tool_text(&response);
    assert!(
        !is_error,
        "agent action should return structured denial (not error): {response:#}"
    );
    assert!(
        text.contains("authorization_denied"),
        "should get structured authorization_denied status, got: {text}"
    );
    assert!(
        text.contains("poll_decision"),
        "denial response should mention poll_decision, got: {text}"
    );

    // Step 2: Agent lists ALL decisions (no status filter) to debug.
    let response = rpc(
        &ctx,
        call_tool_request(22, "execute", "return await temper.get_decisions('demo')"),
    )
    .await;
    let (text, is_error) = tool_text(&response);
    assert!(!is_error, "get_decisions should succeed: {response:#}");
    let decisions: Value = serde_json::from_str(text).expect("json");
    let decisions_arr = decisions
        .get("decisions")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("decisions should have 'decisions' array, got: {text}"));
    assert!(
        !decisions_arr.is_empty(),
        "should have at least one pending decision"
    );
    let decision_id = decisions_arr[0]["id"]
        .as_str()
        .expect("decision should have id");
    assert!(
        decision_id.starts_with("PD-"),
        "decision ID should start with PD-: {decision_id}"
    );

    // Step 2b: Verify agent cannot self-approve.
    let approve_code = format!(
        "return await temper.approve_decision('demo', '{}', 'broad')",
        decision_id
    );
    let response = rpc(&ctx, call_tool_request(22, "execute", &approve_code)).await;
    let (text, is_error) = tool_text(&response);
    assert!(is_error, "approve_decision should be blocked: {response:#}");
    assert!(
        text.contains("not available to agents"),
        "should get governance write blocked message, got: {text}"
    );

    // Step 3: Human approves directly via HTTP (simulating Observe UI / temper decide).
    let http = reqwest::Client::new();
    let approve_url =
        format!("http://127.0.0.1:{port}/api/tenants/demo/decisions/{decision_id}/approve");
    let approve_resp = http
        .post(&approve_url)
        .header("X-Temper-Principal-Kind", "admin")
        .header("X-Temper-Principal-Id", "human-test")
        .json(&json!({
            "scope": {
                "principal": "any_agent",
                "action": "all_actions",
                "resource": "any_resource",
                "duration": "always"
            },
            "decided_by": "human-test"
        }))
        .send()
        .await
        .expect("approve request");
    assert!(
        approve_resp.status().is_success(),
        "human approval should succeed: {:?}",
        approve_resp.status()
    );

    // Step 4: Retry the action — should now succeed (Cedar policy was hot-loaded).
    let response = rpc(
        &ctx,
        call_tool_request(
            24,
            "execute",
            r#"
result = await temper.action('demo', 'Orders', 'agent-e2e-1', 'CancelOrder', {'Reason': 'approved'})
return result['status']
"#,
        ),
    )
    .await;

    let _ = shutdown.send(());

    let (text, is_error) = tool_text(&response);
    assert!(
        !is_error,
        "retry action after human approval should succeed: {response:#}"
    );
    let parsed: Value = serde_json::from_str(text).expect("json");
    assert_eq!(parsed, Value::String("Cancelled".to_string()));
}

#[test]
fn format_authz_denied_with_decision_id() {
    let body = r#"{"error":{"code":"AuthorizationDenied","message":"Authorization denied for AddItem on Order('order-123'). Decision PD-abc123 created."}}"#;
    let result = format_authz_denied(body).expect("should parse");
    assert_eq!(
        result["status"].as_str().unwrap(), // ci-ok: test assertion
        "authorization_denied",
        "should have structured status"
    );
    assert_eq!(
        result["decision_id"].as_str().unwrap(), // ci-ok: test assertion
        "PD-abc123",
        "should include decision ID"
    );
    let hint = result["hint"].as_str().unwrap(); // ci-ok: test assertion
    assert!(
        hint.contains("poll_decision"),
        "should include poll_decision guidance"
    );
    assert!(
        hint.contains("PD-abc123"),
        "hint should reference decision ID"
    );
}

#[test]
fn format_authz_denied_without_decision_id() {
    let body = r#"{"error":{"code":"AuthorizationDenied","message":"Authorization denied: no matching permit policy"}}"#;
    let result = format_authz_denied(body).expect("should parse");
    assert_eq!(result["status"].as_str().unwrap(), "authorization_denied"); // ci-ok: test assertion
    assert!(result.get("decision_id").is_none() || result["decision_id"].is_null());
    let hint = result["hint"].as_str().unwrap(); // ci-ok: test assertion
    assert!(hint.contains("poll_decision"));
}

#[test]
fn format_authz_denied_non_matching_body() {
    let body = r#"{"error":{"code":"NotFound","message":"Entity not found"}}"#;
    assert!(format_authz_denied(body).is_none());
}

#[test]
fn format_authz_denied_structured_json_fields() {
    let body = r#"{"error":{"code":"AuthorizationDenied","message":"Authorization denied for SubmitOrder on Order('ord-1'). Decision PD-xyz789 created."}}"#;
    let result = format_authz_denied(body).expect("should parse");

    // New structured fields
    assert_eq!(result["denied"], json!(true), "denied should be true");
    assert!(
        result["reason"]
            .as_str()
            .unwrap() // ci-ok: test assertion
            .contains("Cedar denied"),
        "reason should contain denial text"
    );
    assert_eq!(
        result["pending_decision"].as_str().unwrap(), // ci-ok: test assertion
        "PD-xyz789",
        "pending_decision should contain decision ID"
    );
    let poll_hint = result["poll_hint"].as_str().unwrap(); // ci-ok: test assertion
    assert!(
        poll_hint.contains("poll_decision"),
        "poll_hint should mention poll_decision"
    );
    assert!(
        poll_hint.contains("PD-xyz789"),
        "poll_hint should reference decision ID"
    );

    // Backward-compatible fields still present
    assert_eq!(
        result["status"].as_str().unwrap(), // ci-ok: test assertion
        "authorization_denied"
    );
    assert_eq!(
        result["decision_id"].as_str().unwrap(), // ci-ok: test assertion
        "PD-xyz789"
    );
}

#[test]
fn format_authz_denied_structured_json_without_decision() {
    let body = r#"{"error":{"code":"AuthorizationDenied","message":"No matching permit policy"}}"#;
    let result = format_authz_denied(body).expect("should parse");

    assert_eq!(result["denied"], json!(true));
    assert!(result["reason"].as_str().unwrap().contains("Cedar denied")); // ci-ok: test assertion
    assert!(
        result["pending_decision"].is_null(),
        "pending_decision should be null when no PD- found"
    );
    let poll_hint = result["poll_hint"].as_str().unwrap(); // ci-ok: test assertion
    assert!(
        poll_hint.contains("poll_decision"),
        "poll_hint should still mention poll_decision"
    );
}

#[tokio::test]
#[ignore = "requires Cedar agent default-deny policy in test server setup"]
async fn get_decision_status_returns_decision() {
    let (port, shutdown) = start_test_temper_server().await;

    // Use agent identity so Cedar authorization applies (default-deny for agents).
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(port),
        temper_url: None,
        principal_id: Some("status-bot".to_string()),
        api_key: None,
    })
    .expect("ctx");

    // Create entity first (creation bypasses Cedar).
    let response = rpc(
        &ctx,
        call_tool_request(
            60,
            "execute",
            "return await temper.create('demo', 'Orders', {'id': 'status-test-1', 'customer': 'Bob'})",
        ),
    )
    .await;
    let (_, is_error) = tool_text(&response);
    assert!(!is_error, "create should succeed: {response:#}");

    // Agent tries an action -- should be denied.
    let response = rpc(
        &ctx,
        call_tool_request(
            61,
            "execute",
            "return await temper.action('demo', 'Orders', 'status-test-1', 'CancelOrder', {'Reason': 'test'})",
        ),
    )
    .await;
    let (text, is_error) = tool_text(&response);
    assert!(!is_error, "should get structured denial: {response:#}");
    let denial: Value = serde_json::from_str(text).expect("json");
    let decision_id = denial["pending_decision"]
        .as_str()
        .or_else(|| denial["decision_id"].as_str())
        .expect("should have decision ID");

    // Use get_decision_status to check.
    let status_code = format!(
        "return await temper.get_decision_status('demo', '{}')",
        decision_id
    );
    let response = rpc(&ctx, call_tool_request(62, "execute", &status_code)).await;

    let _ = shutdown.send(());

    let (text, is_error) = tool_text(&response);
    assert!(
        !is_error,
        "get_decision_status should succeed: {response:#}"
    );
    let status_result: Value = serde_json::from_str(text).expect("json");
    assert_eq!(
        status_result["decision_id"].as_str().unwrap(), // ci-ok: test assertion
        decision_id,
        "should return the correct decision ID"
    );
    assert_eq!(
        status_result["status"].as_str().unwrap(), // ci-ok: test assertion
        "pending",
        "status should be pending"
    );
}
