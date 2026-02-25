use super::*;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use axum::Router;
use serde_json::{Value, json};
use temper_runtime::ActorSystem;
use temper_server::ServerState;
use temper_spec::parse_csdl;
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
    let csdl_xml = include_str!("../../../test-fixtures/specs/model.csdl.xml");
    let csdl = parse_csdl(csdl_xml).expect("parse csdl");

    let mut ioa_sources = BTreeMap::new();
    ioa_sources.insert(
        "Order".to_string(),
        include_str!("../../../test-fixtures/specs/order.ioa.toml").to_string(),
    );

    let state = ServerState::with_specs(
        ActorSystem::new("temper-mcp-tests"),
        csdl,
        csdl_xml.to_string(),
        ioa_sources,
    );

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
        temper_port: 3001,
        apps: vec![],
    })
    .expect("ctx");

    let response = rpc(
        &ctx,
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-05"
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
        temper_port: 3001,
        apps: vec![app("demo", tmp.path())],
    })
    .expect("ctx");

    let response = rpc(
            &ctx,
            call_tool_request(
                2,
                "search",
                "return [a['name'] for e in spec['demo']['entities'].values() for a in e['actions'] if a['name'] == 'SubmitOrder']",
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
        temper_port: port,
        apps: vec![app("demo", tmp.path())],
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
        temper_port: port,
        apps: vec![app("demo", tmp.path())],
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
        temper_port: 3001,
        apps: vec![app("demo", tmp.path())],
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
        temper_port: port,
        apps: vec![app("demo", tmp.path())],
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
