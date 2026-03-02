use super::sandbox::format_authz_denied;
use super::*;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use std::sync::Arc;

use axum::Router;
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_server::{ServerEventStore, ServerState};
use temper_spec::parse_csdl;
use temper_store_turso::TursoEventStore;
use tempfile::tempdir;
use tokio::net::TcpListener;
use tokio::sync::oneshot;

fn write_temp_specs() -> tempfile::TempDir {
    let dir = tempdir().expect("tempdir");
    let specs = dir.path();

    fs::write(
        specs.join("order.ioa.toml"),
        include_str!("../../../test-fixtures/specs/order.ioa.toml"),
    )
    .expect("write ioa");

    fs::write(
        specs.join("model.csdl.xml"),
        include_str!("../../../test-fixtures/specs/model.csdl.xml"),
    )
    .expect("write csdl");

    dir
}

fn app(name: &str, specs_dir: &Path) -> AppConfig {
    AppConfig {
        name: name.to_string(),
        specs_dir: specs_dir.to_path_buf(),
    }
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
    let db_url = format!(
        "file:/tmp/temper-mcp-test-{}-{}.db",
        std::process::id(),
        id,
    );
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
    );
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
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(3001),
        apps: vec![],
        principal_id: None,
    })
    .expect("ctx");

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
async fn search_returns_filtered_spec_data() {
    let tmp = write_temp_specs();
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(3001),
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

    // Use the new spec Dataclass API: spec.actions() returns action list (await required)
    let response = rpc(
        &ctx,
        call_tool_request(
            2,
            "search",
            "actions = await spec.actions('demo', 'Order')\nreturn [a['name'] for a in actions if a['name'] == 'SubmitOrder']",
        ),
    )
    .await;

    let (text, is_error) = tool_text(&response);
    assert!(!is_error, "search should succeed: {response:#}");
    let parsed: Value = serde_json::from_str(text).expect("tool text should be json");
    assert_eq!(parsed, json!(["SubmitOrder"]));
}

#[tokio::test]
async fn execute_creates_entity_and_reads_it_back() {
    let tmp = write_temp_specs();
    let (port, shutdown) = start_test_temper_server().await;
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(port),
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

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
    let tmp = write_temp_specs();
    let (port, shutdown) = start_test_temper_server().await;
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(port),
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

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
async fn sandbox_blocks_filesystem_access() {
    let tmp = write_temp_specs();
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(3001),
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

    let response = rpc(
        &ctx,
        call_tool_request(5, "search", "import os\nreturn open('/etc/passwd').read()"),
    )
    .await;

    let (text, is_error) = tool_text(&response);
    assert!(is_error, "search should fail for filesystem access");
    assert!(
        text.contains("blocked OS access")
            || text.contains("sandbox")
            || text.contains("NameError")
            || text.contains("open"),
        "expected sandbox error, got: {text}"
    );
}

#[tokio::test]
async fn execute_supports_compound_operation() {
    let tmp = write_temp_specs();
    let (port, shutdown) = start_test_temper_server().await;
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(port),
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

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
async fn search_spec_tenants() {
    let tmp = write_temp_specs();
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(3001),
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

    let response = rpc(
        &ctx,
        call_tool_request(7, "search", "return await spec.tenants()"),
    )
    .await;

    let (text, is_error) = tool_text(&response);
    assert!(!is_error, "spec.tenants() should succeed: {response:#}");
    let parsed: Value = serde_json::from_str(text).expect("json");
    assert_eq!(parsed, json!(["demo"]));
}

#[tokio::test]
async fn search_spec_entities() {
    let tmp = write_temp_specs();
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(3001),
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

    let response = rpc(
        &ctx,
        call_tool_request(8, "search", "return await spec.entities('demo')"),
    )
    .await;

    let (text, is_error) = tool_text(&response);
    assert!(!is_error, "spec.entities() should succeed: {response:#}");
    let parsed: Value = serde_json::from_str(text).expect("json");
    let entities = parsed.as_array().expect("should be array");
    assert!(
        entities.contains(&json!("Order")),
        "should include Order: {parsed}"
    );
}

#[tokio::test]
async fn search_spec_describe() {
    let tmp = write_temp_specs();
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(3001),
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

    let response = rpc(
        &ctx,
        call_tool_request(
            9,
            "search",
            "desc = await spec.describe('demo', 'Order')\nreturn list(desc.keys())",
        ),
    )
    .await;

    let (text, is_error) = tool_text(&response);
    assert!(!is_error, "spec.describe() should succeed: {response:#}");
    let parsed: Value = serde_json::from_str(text).expect("json");
    let keys = parsed.as_array().expect("should be array");
    assert!(keys.contains(&json!("states")), "should have states key");
    assert!(keys.contains(&json!("actions")), "should have actions key");
    assert!(keys.contains(&json!("vars")), "should have vars key");
}

#[tokio::test]
async fn search_spec_actions_from() {
    let tmp = write_temp_specs();
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(3001),
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

    let response = rpc(
        &ctx,
        call_tool_request(
            10,
            "search",
            "actions = await spec.actions_from('demo', 'Order', 'Draft')\nreturn [a['name'] for a in actions]",
        ),
    )
    .await;

    let (text, is_error) = tool_text(&response);
    assert!(
        !is_error,
        "spec.actions_from() should succeed: {response:#}"
    );
    let parsed: Value = serde_json::from_str(text).expect("json");
    let action_names = parsed.as_array().expect("should be array");
    // Draft state should have SubmitOrder and CancelOrder as available actions
    assert!(
        !action_names.is_empty(),
        "should have actions available from Draft"
    );
}

#[tokio::test]
async fn tool_list_includes_loaded_summary() {
    let tmp = write_temp_specs();
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(3001),
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

    let response = rpc(
        &ctx,
        json!({
            "jsonrpc": "2.0",
            "id": 11,
            "method": "tools/list"
        }),
    )
    .await;

    let tools = response["result"]["tools"].as_array().expect("tools array");
    let search_desc = tools[0]["description"].as_str().expect("search desc");
    let execute_desc = tools[1]["description"].as_str().expect("execute desc");

    assert!(
        search_desc.contains("Loaded: demo"),
        "search description should include loaded summary: {search_desc}"
    );
    assert!(
        execute_desc.contains("Loaded: demo"),
        "execute description should include loaded summary: {execute_desc}"
    );
    assert!(
        search_desc.contains("Order"),
        "search description should list entity types: {search_desc}"
    );
}

#[tokio::test]
async fn execute_show_spec_returns_spec_data() {
    let tmp = write_temp_specs();
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(3001),
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

    let response = rpc(
        &ctx,
        call_tool_request(
            12,
            "execute",
            "spec = await temper.show_spec('demo', 'Order')\nreturn list(spec.keys())",
        ),
    )
    .await;

    let (text, is_error) = tool_text(&response);
    assert!(!is_error, "show_spec should succeed: {response:#}");
    let parsed: Value = serde_json::from_str(text).expect("json");
    let keys = parsed.as_array().expect("should be array");
    assert!(keys.contains(&json!("states")), "should have states");
    assert!(keys.contains(&json!("actions")), "should have actions");
}

/// End-to-end governance flow: agent denied → get decision → human approves
/// directly (not via agent sandbox) → agent retries → success.
#[tokio::test]
async fn e2e_agent_denial_human_approve_retry() {
    let tmp = write_temp_specs();
    let (port, shutdown) = start_test_temper_server().await;

    // Use agent identity so Cedar authorization applies (default-deny for agents).
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(port),
        apps: vec![app("demo", tmp.path())],
        principal_id: Some("checkout-bot".to_string()),
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
    // Verify enhanced error message includes poll_decision guidance.
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
        .json(&json!({ "scope": "broad", "decided_by": "human-test" }))
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

/// Verify search spec methods work end-to-end with multiple chained calls.
#[tokio::test]
async fn e2e_search_chained_discovery() {
    let tmp = write_temp_specs();
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(3001),
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

    // Chain: tenants → entities → describe → actions_from in one search call.
    let response = rpc(
        &ctx,
        call_tool_request(
            30,
            "search",
            r#"
tenants = await spec.tenants()
entities = await spec.entities(tenants[0])
desc = await spec.describe(tenants[0], entities[0])
draft_actions = await spec.actions_from(tenants[0], entities[0], desc['initial'])
return {
    'tenant': tenants[0],
    'entity': entities[0],
    'states': desc['states'],
    'initial': desc['initial'],
    'draft_action_count': len(draft_actions),
}
"#,
        ),
    )
    .await;

    let (text, is_error) = tool_text(&response);
    assert!(!is_error, "chained search should succeed: {response:#}");
    let result: Value = serde_json::from_str(text).expect("json");
    assert_eq!(result["tenant"], "demo");
    assert_eq!(result["entity"], "Order");
    assert_eq!(result["initial"], "Draft");
    assert!(
        result["draft_action_count"].as_i64().unwrap_or(0) > 0,
        "should have actions from Draft state: {result}"
    );
}

/// Verify execute show_spec matches search describe for the same entity.
#[tokio::test]
async fn e2e_show_spec_matches_search_describe() {
    let tmp = write_temp_specs();
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(3001),
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

    // Get spec via search (spec.describe)
    let search_resp = rpc(
        &ctx,
        call_tool_request(40, "search", "return await spec.describe('demo', 'Order')"),
    )
    .await;
    let (search_text, search_err) = tool_text(&search_resp);
    assert!(!search_err, "search describe failed: {search_resp:#}");

    // Get spec via execute (temper.show_spec)
    let exec_resp = rpc(
        &ctx,
        call_tool_request(
            41,
            "execute",
            "return await temper.show_spec('demo', 'Order')",
        ),
    )
    .await;
    let (exec_text, exec_err) = tool_text(&exec_resp);
    assert!(!exec_err, "show_spec failed: {exec_resp:#}");

    // They should return the same data.
    let search_val: Value = serde_json::from_str(search_text).expect("json");
    let exec_val: Value = serde_json::from_str(exec_text).expect("json");
    assert_eq!(
        search_val, exec_val,
        "spec.describe and temper.show_spec should return identical data"
    );
}

#[tokio::test]
async fn execute_without_server_returns_helpful_error() {
    let tmp = write_temp_specs();
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: None,
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

    let response = rpc(
        &ctx,
        call_tool_request(50, "execute", "return await temper.list('demo', 'Order')"),
    )
    .await;

    let (text, is_error) = tool_text(&response);
    assert!(is_error, "execute without server should fail");
    assert!(
        text.contains("Server not running"),
        "should get helpful error message, got: {text}"
    );
    assert!(
        text.contains("start_server"),
        "should mention start_server, got: {text}"
    );
}

#[tokio::test]
async fn search_works_without_server() {
    let tmp = write_temp_specs();
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: None,
        apps: vec![app("demo", tmp.path())],
        principal_id: None,
    })
    .expect("ctx");

    let response = rpc(
        &ctx,
        call_tool_request(51, "search", "return await spec.tenants()"),
    )
    .await;

    let (text, is_error) = tool_text(&response);
    assert!(!is_error, "search should work without server: {response:#}");
    let parsed: Value = serde_json::from_str(text).expect("json");
    assert_eq!(parsed, json!(["demo"]));
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
async fn get_decision_status_returns_decision() {
    let tmp = write_temp_specs();
    let (port, shutdown) = start_test_temper_server().await;

    // Use agent identity so Cedar authorization applies (default-deny for agents).
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(port),
        apps: vec![app("demo", tmp.path())],
        principal_id: Some("status-bot".to_string()),
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
