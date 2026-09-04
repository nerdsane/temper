//! End-to-end tests for elicitation approvals (ADR-0173): the full MCP loop
//! run over in-memory pipes against a scripted fake client and a mock Temper
//! backend that denies actions with a pending decision.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream};

use crate::McpConfig;
use crate::runtime::{RuntimeContext, run_loop};

const OPERATOR_KEY: &str = "elicit-test-operator-key";
const DENIAL_BODY: &str = r#"{"error":{"code":"AuthorizationDenied","message":"Authorization denied for CancelOrder on Order('o1'). Decision PD-test123 created."}}"#;

/// One resolution call captured by the mock backend.
#[derive(Clone, Debug)]
struct CapturedResolution {
    tenant: String,
    decision_id: String,
    authorization: Option<String>,
    x_tenant_id: Option<String>,
    body: Option<Value>,
}

#[derive(Default)]
struct MockBackend {
    approve: Mutex<Option<CapturedResolution>>,
    deny: Mutex<Option<CapturedResolution>>,
}

fn capture(
    tenant: String,
    decision_id: String,
    headers: &HeaderMap,
    body: Option<Value>,
) -> CapturedResolution {
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };
    CapturedResolution {
        tenant,
        decision_id,
        authorization: header("authorization"),
        x_tenant_id: header("x-tenant-id"),
        body,
    }
}

async fn handle_approve(
    State(backend): State<Arc<MockBackend>>,
    Path((tenant, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> impl IntoResponse {
    let captured = capture(tenant, id.clone(), &headers, body.map(|Json(v)| v));
    *backend.approve.lock().expect("approve lock") = Some(captured);
    (
        StatusCode::OK,
        Json(json!({"id": id, "status": "approved", "generated_policy": "permit(...);"})),
    )
}

async fn handle_deny(
    State(backend): State<Arc<MockBackend>>,
    Path((tenant, id)): Path<(String, String)>,
    headers: HeaderMap,
    body: Option<Json<Value>>,
) -> impl IntoResponse {
    let captured = capture(tenant, id.clone(), &headers, body.map(|Json(v)| v));
    *backend.deny.lock().expect("deny lock") = Some(captured);
    (StatusCode::OK, Json(json!({"id": id, "status": "denied"})))
}

/// Everything else — including the denied `temper.action` POST — returns the
/// structured Cedar denial with a pending decision id.
async fn handle_denied() -> impl IntoResponse {
    (
        StatusCode::FORBIDDEN,
        [("content-type", "application/json")],
        DENIAL_BODY,
    )
}

/// Start the mock Temper backend; returns its port and captured state.
async fn start_mock_backend() -> (u16, Arc<MockBackend>) {
    let backend = Arc::new(MockBackend::default());
    let app = Router::new()
        .route(
            "/api/tenants/{tenant}/decisions/{id}/approve",
            post(handle_approve),
        )
        .route(
            "/api/tenants/{tenant}/decisions/{id}/deny",
            post(handle_deny),
        )
        .fallback(handle_denied)
        .with_state(backend.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock backend");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("serve mock backend");
    });
    (port, backend)
}

struct FakeClient {
    writer: DuplexStream,
    reader: BufReader<DuplexStream>,
}

impl FakeClient {
    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_string(&message).expect("encode");
        line.push('\n');
        self.writer
            .write_all(line.as_bytes())
            .await
            .expect("client write");
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        let read = tokio::time::timeout(Duration::from_secs(20), self.reader.read_line(&mut line))
            .await
            .expect("server produced a message in time")
            .expect("client read");
        assert!(read > 0, "server closed the stream unexpectedly");
        serde_json::from_str(line.trim()).expect("server sent valid JSON")
    }

    async fn initialize(&mut self, with_elicitation: bool) {
        let capabilities = if with_elicitation {
            json!({"elicitation": {}})
        } else {
            json!({})
        };
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-06-18",
                "capabilities": capabilities,
                "clientInfo": {"name": "fake-client", "version": "0.0.1"}
            }
        }))
        .await;
        let response = self.recv().await;
        assert_eq!(response["id"], 1, "initialize response");
        assert_eq!(response["result"]["protocolVersion"], "2025-06-18");
    }

    async fn call_denied_action(&mut self) {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/call",
            "params": {
                "name": "execute",
                "arguments": {
                    "code": "return await temper.action('demo', 'Orders', 'o1', 'CancelOrder', {'Reason': 'test'})"
                }
            }
        }))
        .await;
    }
}

/// Spawn the server loop over in-memory pipes and hand back the fake client.
fn wire_session(port: u16) -> (impl Future<Output = anyhow::Result<()>>, FakeClient) {
    let ctx = RuntimeContext::from_config(&McpConfig {
        temper_port: Some(port),
        temper_url: None,
        agent_id: None,
        agent_type: None,
        session_id: Some("elicit-test-session".to_string()),
        api_key: Some(OPERATOR_KEY.to_string()),
    })
    .expect("ctx");

    let (client_to_server_tx, client_to_server_rx) = tokio::io::duplex(1 << 20);
    let (server_to_client_tx, server_to_client_rx) = tokio::io::duplex(1 << 20);
    let server = run_loop(
        ctx,
        BufReader::new(client_to_server_rx),
        server_to_client_tx,
    );
    let client = FakeClient {
        writer: client_to_server_tx,
        reader: BufReader::new(server_to_client_rx),
    };
    (server, client)
}

fn tool_result_json(response: &Value) -> Value {
    let text = response
        .pointer("/result/content/0/text")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("tool result text missing: {response:#}"));
    serde_json::from_str(text).unwrap_or_else(|_| panic!("tool text not JSON: {text}"))
}

#[tokio::test]
async fn denial_with_elicitation_capability_approve_flow() {
    let (port, backend) = start_mock_backend().await;
    let (server, mut client) = wire_session(port);

    let script = async move {
        client.initialize(true).await;
        client.call_denied_action().await;

        // The server must pause the tool result and elicit the human first.
        let elicitation = client.recv().await;
        assert_eq!(
            elicitation["method"], "elicitation/create",
            "expected an elicitation before the tool result: {elicitation:#}"
        );
        let message = elicitation["params"]["message"].as_str().expect("message");
        assert!(message.contains("PD-test123"), "message names the decision");
        let schema = &elicitation["params"]["requestedSchema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(
            schema["properties"]["decision"]["enum"],
            json!(["approve_narrow", "approve_broad", "deny", "leave_pending"])
        );

        // The human approves narrowly.
        client
            .send(json!({
                "jsonrpc": "2.0",
                "id": elicitation["id"],
                "result": {"action": "accept", "content": {"decision": "approve_narrow"}}
            }))
            .await;

        // The tool result arrives annotated for the model to retry.
        let response = client.recv().await;
        assert_eq!(response["id"], 2, "tools/call response");
        let result = tool_result_json(&response);
        assert_eq!(result["status"], "authorization_denied");
        assert_eq!(result["approval"], "granted by human via elicitation");
        assert_eq!(result["decision_id"], "PD-test123");
        assert_eq!(result["approved_scope"], "narrow");
        assert!(
            result["retry"]
                .as_str()
                .expect("retry")
                .contains("re-invoke"),
            "annotation tells the model to retry"
        );
        drop(client);
    };

    let (server_result, ()) = tokio::join!(server, script);
    server_result.expect("server loop");

    // The approve endpoint was hit with the operator credential and scope.
    let approve = backend
        .approve
        .lock()
        .expect("approve lock")
        .clone()
        .expect("approve endpoint was called");
    assert_eq!(approve.tenant, "demo");
    assert_eq!(approve.decision_id, "PD-test123");
    assert_eq!(
        approve.authorization.as_deref(),
        Some(concat!("Bearer ", "elicit-test-operator-key"))
    );
    assert_eq!(approve.x_tenant_id.as_deref(), Some("demo"));
    let scope = approve.body.expect("approve body")["scope"].clone();
    let matrix: temper_authz::PolicyScopeMatrix =
        serde_json::from_value(scope).expect("scope deserializes into PolicyScopeMatrix");
    assert_eq!(matrix.principal, temper_authz::PrincipalScope::ThisAgent);
    assert_eq!(matrix.action, temper_authz::ActionScope::ThisAction);
    assert_eq!(matrix.resource, temper_authz::ResourceScope::ThisResource);
    assert!(
        backend.deny.lock().expect("deny lock").is_none(),
        "deny endpoint must not be called"
    );
}

#[tokio::test]
async fn denial_without_elicitation_capability_passes_through() {
    let (port, backend) = start_mock_backend().await;
    let (server, mut client) = wire_session(port);

    let script = async move {
        client.initialize(false).await;
        client.call_denied_action().await;

        // The very next message must be the tool result — no elicitation.
        let response = client.recv().await;
        assert_eq!(
            response["id"], 2,
            "expected the tools/call response, not an elicitation: {response:#}"
        );
        let result = tool_result_json(&response);
        assert_eq!(result["status"], "authorization_denied");
        assert_eq!(result["decision_id"], "PD-test123");
        assert!(
            result.get("approval").is_none(),
            "denial must pass through untouched: {result:#}"
        );
        drop(client);
    };

    let (server_result, ()) = tokio::join!(server, script);
    server_result.expect("server loop");

    assert!(
        backend.approve.lock().expect("approve lock").is_none(),
        "approve endpoint must not be called without the capability"
    );
    assert!(backend.deny.lock().expect("deny lock").is_none());
}

#[tokio::test]
async fn denial_with_elicitation_human_denies() {
    let (port, backend) = start_mock_backend().await;
    let (server, mut client) = wire_session(port);

    let script = async move {
        client.initialize(true).await;
        client.call_denied_action().await;

        let elicitation = client.recv().await;
        assert_eq!(elicitation["method"], "elicitation/create");
        client
            .send(json!({
                "jsonrpc": "2.0",
                "id": elicitation["id"],
                "result": {"action": "accept", "content": {"decision": "deny"}}
            }))
            .await;

        let response = client.recv().await;
        assert_eq!(response["id"], 2);
        let result = tool_result_json(&response);
        assert_eq!(result["approval"], "denied by human via elicitation");
        assert_eq!(result["decision_id"], "PD-test123");
        drop(client);
    };

    let (server_result, ()) = tokio::join!(server, script);
    server_result.expect("server loop");

    let deny = backend
        .deny
        .lock()
        .expect("deny lock")
        .clone()
        .expect("deny endpoint was called");
    assert_eq!(deny.tenant, "demo");
    assert_eq!(deny.decision_id, "PD-test123");
    assert_eq!(
        deny.authorization.as_deref(),
        Some(concat!("Bearer ", "elicit-test-operator-key"))
    );
    assert!(
        backend.approve.lock().expect("approve lock").is_none(),
        "approve endpoint must not be called on a human deny"
    );
}

#[tokio::test]
async fn denial_with_elicitation_human_declines_leaves_pending() {
    let (port, backend) = start_mock_backend().await;
    let (server, mut client) = wire_session(port);

    let script = async move {
        client.initialize(true).await;
        client.call_denied_action().await;

        let elicitation = client.recv().await;
        assert_eq!(elicitation["method"], "elicitation/create");
        client
            .send(json!({
                "jsonrpc": "2.0",
                "id": elicitation["id"],
                "result": {"action": "decline"}
            }))
            .await;

        let response = client.recv().await;
        assert_eq!(response["id"], 2);
        let result = tool_result_json(&response);
        assert_eq!(result["status"], "authorization_denied");
        assert!(
            result.get("approval").is_none(),
            "a decline must leave the denial untouched: {result:#}"
        );
        drop(client);
    };

    let (server_result, ()) = tokio::join!(server, script);
    server_result.expect("server loop");

    assert!(
        backend.approve.lock().expect("approve lock").is_none(),
        "a decline must never resolve the decision"
    );
    assert!(backend.deny.lock().expect("deny lock").is_none());
}

#[tokio::test]
async fn client_disconnect_mid_elicitation_ends_promptly_without_resolution() {
    let (port, backend) = start_mock_backend().await;
    let (server, mut client) = wire_session(port);

    let script = async move {
        client.initialize(true).await;
        client.call_denied_action().await;

        let elicitation = client.recv().await;
        assert_eq!(elicitation["method"], "elicitation/create");
        // The client goes away without answering.
        drop(client);
    };

    // The session must end well before the 120s elicitation timeout: the
    // reader fails the pending request on EOF instead of waiting it out.
    let (server_result, ()) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(server, script)
    })
    .await
    .expect("session ends promptly after client disconnect");
    server_result.expect("server loop");

    assert!(
        backend.approve.lock().expect("approve lock").is_none(),
        "a disconnect must never resolve the decision"
    );
    assert!(backend.deny.lock().expect("deny lock").is_none());
}
