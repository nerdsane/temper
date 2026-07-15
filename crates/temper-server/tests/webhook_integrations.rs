//! ARN-227: spec-declared webhook integrations must actually execute.
//!
//! An IOA spec can declare `[[integration]]` blocks with `type = "webhook"`
//! (the DEFAULT integration type). They parse, verify ("metadata only"), and
//! deploy — but the runtime executed only `type = "wasm"` integrations, and
//! the separate `WebhookDispatcher` fires only from the server-level
//! `webhooks.toml`. A developer's declared webhook was silently dead
//! configuration: accepted everywhere, executed nowhere.

use std::sync::Arc;
use std::time::Duration;

use temper_runtime::{ActorSystem, TenantId};
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

const ORDER_CSDL: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<edmx:Edmx Version="4.0" xmlns:edmx="http://docs.oasis-open.org/odata/ns/edmx">
  <edmx:DataServices>
    <Schema Namespace="Temper.Arn227" xmlns="http://docs.oasis-open.org/odata/ns/edm">
      <EntityType Name="Order">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.String" Nullable="false"/>
      </EntityType>
      <EntityContainer Name="Container">
        <EntitySet Name="Orders" EntityType="Temper.Arn227.Order"/>
      </EntityContainer>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>"#;

fn order_spec_with_webhook_integration(url: &str) -> String {
    format!(
        r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
kind = "input"
from = ["Draft"]
to = "Submitted"
hint = "Submit the order."

[[integration]]
name = "notify_fulfillment"
trigger = "SubmitOrder"
type = "webhook"
url = "{url}"
"#
    )
}

/// Bind a local listener that captures the first HTTP request it receives
/// (start-line + headers + body as one string) and returns 200.
async fn capturing_listener() -> (String, Arc<Mutex<Option<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let capture = Arc::clone(&captured);

    tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let mut buf = vec![0u8; 16 * 1024];
            let mut collected = Vec::new();
            // Read until headers AND the content-length body are complete
            // (bounded by the 500ms idle timeout per read), so a client that
            // sends headers and body in separate writes is still captured
            // fully.
            loop {
                match tokio::time::timeout(Duration::from_millis(500), stream.read(&mut buf)).await
                {
                    Ok(Ok(n)) if n > 0 => collected.extend_from_slice(&buf[..n]),
                    _ => break,
                }
                let Some(headers_end) = collected
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|p| p + 4)
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&collected[..headers_end]).to_lowercase();
                let content_length: usize = headers
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
                if collected.len() >= headers_end + content_length {
                    break;
                }
            }
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n")
                .await;
            *capture.lock().await = Some(String::from_utf8_lossy(&collected).to_string());
        }
    });

    (format!("http://{addr}/hook"), captured)
}

/// A spec-declared webhook integration must fire an HTTP request when its
/// trigger action executes successfully.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_webhook_integration_fires_on_its_trigger_action() {
    let (url, captured) = capturing_listener().await;

    let mut registry = SpecRegistry::new();
    let csdl = temper_spec::parse_csdl(ORDER_CSDL).expect("csdl parses");
    let spec_toml = order_spec_with_webhook_integration(&url);
    registry.register_tenant(
        "arn227",
        csdl,
        ORDER_CSDL.to_string(),
        &[("Order", &spec_toml)],
    );

    let system = ActorSystem::new("arn227-test");
    let state = ServerState::from_registry(system, registry);
    let tenant = TenantId::new("arn227");

    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "ord-1",
            "SubmitOrder",
            serde_json::json!({}),
            &AgentContext::for_service("arn227-test"),
        )
        .await
        .expect("dispatch succeeds");
    assert!(response.success, "the trigger action itself must succeed");

    // Bounded wait for the fire-and-forget webhook.
    let mut request = None;
    for _ in 0..100 {
        if let Some(r) = captured.lock().await.clone() {
            request = Some(r);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let request = request.expect(
        "the declared webhook integration must fire an HTTP request when its \
         trigger action executes — declared-and-verified configuration must \
         not be silently dead",
    );
    assert!(
        request.starts_with("POST /hook"),
        "webhook must POST to the declared url, got: {request}"
    );
    assert!(
        request.contains("SubmitOrder"),
        "webhook payload must identify the triggering action, got: {request}"
    );
}

/// The integration fires only for ITS trigger: a different action on the same
/// entity type must not call the webhook.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn declared_webhook_integration_does_not_fire_for_other_actions() {
    let (url, captured) = capturing_listener().await;

    let spec_toml = format!(
        r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
kind = "input"
from = ["Draft"]
to = "Submitted"
hint = "Submit the order."

[[action]]
name = "Touch"
kind = "input"
from = ["Draft"]
to = "Draft"
hint = "No-op touch."

[[integration]]
name = "notify_fulfillment"
trigger = "SubmitOrder"
type = "webhook"
url = "{url}"
"#
    );

    let mut registry = SpecRegistry::new();
    let csdl = temper_spec::parse_csdl(ORDER_CSDL).expect("csdl parses");
    registry.register_tenant(
        "arn227",
        csdl,
        ORDER_CSDL.to_string(),
        &[("Order", &spec_toml)],
    );

    let system = ActorSystem::new("arn227-neg-test");
    let state = ServerState::from_registry(system, registry);
    let tenant = TenantId::new("arn227");

    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "ord-2",
            "Touch",
            serde_json::json!({}),
            &AgentContext::for_service("arn227-test"),
        )
        .await
        .expect("dispatch succeeds");
    assert!(response.success);

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        captured.lock().await.is_none(),
        "an integration must fire only for its declared trigger action"
    );
}

/// ADR-0046 trigger-synthesized webhook integrations (from `[[action.triggers]]`
/// blocks) carry `header.{Name}` and `body_template` config keys. The
/// dispatcher must honor that contract: `header.X-Api-Key` becomes the HTTP
/// header `X-Api-Key` (not a literal "header.x-api-key"), and `body_template`
/// becomes the request body with `${...}` variables expanded — never a header.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn trigger_synthesized_webhook_honors_header_and_body_contract() {
    let (url, captured) = capturing_listener().await;

    let spec_toml = format!(
        r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
kind = "input"
from = ["Draft"]
to = "Submitted"
hint = "Submit the order."

[[action.triggers]]
name = "notify"
kind = "webhook"
url = "{url}"
method = "POST"
body_template = "order ${{entity_id}} moved to ${{to_status}}"

[action.triggers.headers]
X-Api-Key = "test-key-123"
"#
    );

    let mut registry = SpecRegistry::new();
    let csdl = temper_spec::parse_csdl(ORDER_CSDL).expect("csdl parses");
    registry.register_tenant(
        "arn227",
        csdl,
        ORDER_CSDL.to_string(),
        &[("Order", &spec_toml)],
    );

    let system = ActorSystem::new("arn227-synth-test");
    let state = ServerState::from_registry(system, registry);
    let tenant = TenantId::new("arn227");

    let response = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "ord-9",
            "SubmitOrder",
            serde_json::json!({}),
            &AgentContext::for_service("arn227-test"),
        )
        .await
        .expect("dispatch succeeds");
    assert!(response.success);

    let mut request = None;
    for _ in 0..100 {
        if let Some(r) = captured.lock().await.clone() {
            request = Some(r);
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let request = request.expect("the trigger-synthesized webhook must fire");

    let request_lower = request.to_lowercase();
    assert!(
        request_lower.contains("x-api-key: test-key-123"),
        "header.X-Api-Key must arrive as the header X-Api-Key, got: {request}"
    );
    assert!(
        !request_lower.contains("header.x-api-key"),
        "the header.-prefixed config key must never be sent literally, got: {request}"
    );
    assert!(
        !request_lower.contains("body_template:"),
        "body_template must never be sent as a header, got: {request}"
    );
    assert!(
        request.contains("order ord-9 moved to Submitted"),
        "body_template must become the expanded request body, got: {request}"
    );
}
