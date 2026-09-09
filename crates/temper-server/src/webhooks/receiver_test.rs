use super::*;
use crate::aws_sigv4::{hex_encode, hmac_sha256};
use crate::secrets::SecretsVault;
use axum::body::Body;
use axum::http::Request;
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");

/// IOA spec with a webhook declaration for OAuth callback.
const ORDER_IOA_WITH_WEBHOOK: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted", "Confirmed", "Cancelled", "Authorized"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
kind = "input"
from = ["Draft"]
to = "Submitted"

[[action]]
name = "ConfirmOrder"
kind = "input"
from = ["Submitted"]
to = "Confirmed"

[[action]]
name = "CancelOrder"
kind = "input"
from = ["Draft", "Submitted"]
to = "Cancelled"

[[action]]
name = "HandleOAuthCallback"
kind = "input"
from = ["Submitted"]
to = "Authorized"
params = ["code"]

[[webhook]]
name = "oauth_callback"
path = "oauth/callback"
method = "GET"
action = "HandleOAuthCallback"
entity_lookup = "query_param"
entity_param = "state"

[webhook.extract]
code = "query.code"
"#;

const ORDER_IOA_WITH_HMAC_WEBHOOK: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted", "Confirmed", "Cancelled", "Authorized"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
kind = "input"
from = ["Draft"]
to = "Submitted"

[[action]]
name = "HandleOAuthCallback"
kind = "input"
from = ["Submitted"]
to = "Authorized"
params = ["code"]

[[webhook]]
name = "oauth_callback"
path = "oauth/callback"
method = "GET"
action = "HandleOAuthCallback"
entity_lookup = "query_param"
entity_param = "state"
hmac_secret = "{secret:WEBHOOK_HMAC}"
hmac_header = "x-webhook-signature"

[webhook.extract]
code = "query.code"
"#;

const WEBHOOK_HMAC_SECRET: &str = "webhook-test-secret";

fn sign_webhook(method: &str, path_and_query: &str, body: &[u8]) -> String {
    let mut payload = Vec::new();
    payload.extend_from_slice(method.as_bytes());
    payload.push(b'\n');
    payload.extend_from_slice(path_and_query.as_bytes());
    payload.push(b'\n');
    payload.extend_from_slice(body);
    hex_encode(&hmac_sha256(WEBHOOK_HMAC_SECRET.as_bytes(), &payload))
}

fn build_test_state() -> ServerState {
    let csdl = parse_csdl(CSDL_XML).unwrap();
    let system = ActorSystem::new("webhook-test");
    let state = ServerState::new(system, csdl, CSDL_XML.to_string());

    // Register tenant with webhook-enabled spec.
    {
        let mut registry = state.registry.write().unwrap();
        let csdl2 = parse_csdl(CSDL_XML).unwrap();
        registry.register_tenant(
            "test-tenant",
            csdl2,
            CSDL_XML.to_string(),
            &[("Order", ORDER_IOA_WITH_WEBHOOK)],
        );
    }

    state
}

fn build_hmac_test_state() -> ServerState {
    let csdl = parse_csdl(CSDL_XML).unwrap();
    let system = ActorSystem::new("webhook-hmac-test");
    let mut state = ServerState::new(system, csdl, CSDL_XML.to_string());
    {
        let mut registry = state.registry.write().unwrap();
        let csdl2 = parse_csdl(CSDL_XML).unwrap();
        registry.register_tenant(
            "test-tenant",
            csdl2,
            CSDL_XML.to_string(),
            &[("Order", ORDER_IOA_WITH_HMAC_WEBHOOK)],
        );
    }
    let vault = SecretsVault::new(&[7u8; 32]);
    vault
        .cache_secret(
            "test-tenant",
            "WEBHOOK_HMAC",
            WEBHOOK_HMAC_SECRET.to_string(),
        )
        .expect("cache webhook secret");
    state = state.with_secrets_vault(vault);
    state
        .authz
        .reload_tenant_policies(
            "test-tenant",
            r#"permit(
                    principal == Agent::"webhook:oauth_callback",
                    action == Action::"HandleOAuthCallback",
                    resource is Order
                );"#,
        )
        .expect("webhook permit should parse");
    state
}

fn build_test_router() -> axum::Router {
    crate::router::build_router(build_test_state())
}

async fn submitted_order(state: &ServerState, entity_id: &str) {
    let tenant = TenantId::new("test-tenant");
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            entity_id,
            serde_json::json!({"id": entity_id}),
        )
        .await
        .expect("entity creation should succeed");
    let submit = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            entity_id,
            "SubmitOrder",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("SubmitOrder should succeed");
    assert!(submit.success, "SubmitOrder should succeed");
    assert_eq!(submit.state.status, "Submitted");
}

#[tokio::test]
async fn webhook_dispatches_action() {
    let state = build_hmac_test_state();
    submitted_order(&state, "ent-1").await;

    let path_and_query = "/webhooks/test-tenant/oauth/callback?state=ent-1&code=abc123";
    let signature = sign_webhook("GET", path_and_query, b"");
    let app = crate::router::build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path_and_query)
                .header("x-webhook-signature", signature)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(
        json["success"].as_bool().unwrap_or(false),
        "HandleOAuthCallback should succeed"
    );
    assert_eq!(json["state"]["status"], "Authorized");
}

#[tokio::test]
async fn webhook_bad_hmac_returns_401() {
    let state = build_hmac_test_state();
    submitted_order(&state, "ent-1").await;
    let app = crate::router::build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/webhooks/test-tenant/oauth/callback?state=ent-1&code=abc123")
                .header("x-webhook-signature", "deadbeef")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn webhook_valid_hmac_without_permit_returns_403() {
    let state = build_hmac_test_state();
    state
        .authz
        .reload_tenant_policies("test-tenant", "")
        .expect("clear webhook permit");
    submitted_order(&state, "ent-1").await;
    let path_and_query = "/webhooks/test-tenant/oauth/callback?state=ent-1&code=abc123";
    let signature = sign_webhook("GET", path_and_query, b"");
    let app = crate::router::build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path_and_query)
                .header("x-webhook-signature", signature)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn webhook_without_hmac_config_returns_401() {
    let state = build_test_state();
    let tenant = TenantId::new("test-tenant");
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            "ent-1",
            serde_json::json!({"id": "ent-1"}),
        )
        .await
        .expect("entity creation should succeed");
    state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "ent-1",
            "SubmitOrder",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await
        .expect("SubmitOrder should succeed");

    let app = crate::router::build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/webhooks/test-tenant/oauth/callback?state=ent-1&code=abc123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn webhook_missing_entity_id_returns_400() {
    let app = build_test_router();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/webhooks/test-tenant/oauth/callback?code=abc123")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn webhook_unknown_path_returns_404() {
    let app = build_test_router();

    let response = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/webhooks/test-tenant/nonexistent/path?entity_id=ent-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn webhook_extracts_query_params() {
    let query: BTreeMap<String, String> = [
        ("code".to_string(), "auth-code-123".to_string()),
        ("state".to_string(), "entity-id".to_string()),
    ]
    .into_iter()
    .collect();

    assert_eq!(
        extract_param("query.code", &query),
        Some("auth-code-123".to_string())
    );
    assert_eq!(
        extract_param("query.state", &query),
        Some("entity-id".to_string())
    );
    assert_eq!(extract_param("query.missing", &query), None);
}
