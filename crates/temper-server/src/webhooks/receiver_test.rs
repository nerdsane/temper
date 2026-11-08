use super::*;
use crate::aws_sigv4::{hex_encode, hmac_sha256};
use crate::secrets::SecretsVault;
use axum::body::Body;
use axum::http::Request;
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");

/// One entity with a webhook per scheme the tests drive: the kernel's own
/// (a GET callback), Stripe's (the body names the record) and GitHub's (no
/// declared method, so any method).
const ORDER_IOA: &str = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted", "Authorized", "Paid"]
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

[[action]]
name = "Pay"
kind = "input"
from = ["Submitted"]
to = "Paid"
params = [{ name = "amount_total", type = "int" }, "event_id", "delivery"]

[[webhook]]
name = "oauth_callback"
path = "oauth/callback"
method = "GET"
action = "HandleOAuthCallback"
hmac_secret = "{secret:WEBHOOK_HMAC}"
hmac_header = "x-webhook-signature"
entity_id = "query.state"

[webhook.extract]
code = "query.code"

[[webhook]]
name = "stripe_paid"
path = "stripe/paid"
action = "Pay"
signature = "stripe"
hmac_secret = "{secret:STRIPE_WEBHOOK_SECRET}"
entity_id = "body.data.object.client_reference_id"

[webhook.extract]
amount_total = "body.data.object.amount_total"
event_id = "body.id"
delivery = "header.x-delivery"

[[webhook]]
name = "github_issue"
path = "github/issue"
action = "Pay"
signature = "github"
hmac_secret = "{secret:GITHUB_WEBHOOK_SECRET}"
entity_id = "body.issue.number"

[webhook.extract]
delivery = "header.x-github-delivery"
"#;

const WEBHOOK_HMAC_SECRET: &str = "webhook-test-secret";
const STRIPE_SECRET: &str = "whsec_stripe_test";
const GITHUB_SECRET: &str = "github-test-secret";

const PERMITS: &str = r#"
permit(principal == Agent::"webhook:oauth_callback", action == Action::"HandleOAuthCallback", resource is Order);
permit(principal == Agent::"webhook:stripe_paid", action == Action::"Pay", resource is Order);
permit(principal == Agent::"webhook:github_issue", action == Action::"Pay", resource is Order);
"#;

fn sign_webhook(method: &str, path_and_query: &str, body: &[u8]) -> String {
    let mut payload = Vec::new();
    payload.extend_from_slice(method.as_bytes());
    payload.push(b'\n');
    payload.extend_from_slice(path_and_query.as_bytes());
    payload.push(b'\n');
    payload.extend_from_slice(body);
    hex_encode(&hmac_sha256(WEBHOOK_HMAC_SECRET.as_bytes(), &payload))
}

/// `Stripe-Signature` for `body`, signed `age_seconds` ago.
fn stripe_signature(body: &[u8], age_seconds: i64) -> String {
    let t = sim_now().timestamp() - age_seconds;
    let mut signed = format!("{t}.").into_bytes();
    signed.extend_from_slice(body);
    format!(
        "t={t},v1={}",
        hex_encode(&hmac_sha256(STRIPE_SECRET.as_bytes(), &signed))
    )
}

fn build_test_state() -> ServerState {
    let csdl = parse_csdl(CSDL_XML).unwrap();
    let system = ActorSystem::new("webhook-test");
    let mut state = ServerState::new(system, csdl, CSDL_XML.to_string());
    {
        let mut registry = state.registry.write().unwrap();
        let csdl2 = parse_csdl(CSDL_XML).unwrap();
        registry.register_tenant(
            "test-tenant",
            csdl2,
            CSDL_XML.to_string(),
            &[("Order", ORDER_IOA)],
        );
    }
    let vault = SecretsVault::new(&[7u8; 32]);
    for (key, value) in [
        ("WEBHOOK_HMAC", WEBHOOK_HMAC_SECRET),
        ("STRIPE_WEBHOOK_SECRET", STRIPE_SECRET),
        ("GITHUB_WEBHOOK_SECRET", GITHUB_SECRET),
    ] {
        vault.cache_secret("test-tenant", key, value.to_string());
    }
    state = state.with_secrets_vault(vault);
    state
        .authz
        .reload_tenant_policies("test-tenant", PERMITS)
        .expect("webhook permits should parse");
    state
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

async fn send(state: ServerState, request: Request<Body>) -> (StatusCode, String) {
    let response = crate::router::build_router(state)
        .oneshot(request)
        .await
        .unwrap();
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

fn stripe_event(client_reference_id: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": "evt_1",
        "type": "checkout.session.completed",
        "data": {"object": {"client_reference_id": client_reference_id, "amount_total": 1999}}
    }))
    .unwrap()
}

fn stripe_post(body: Vec<u8>, signature: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/webhooks/test-tenant/stripe/paid")
        .header("content-type", "application/json")
        .header("stripe-signature", signature)
        .header("x-delivery", "d-1")
        .body(Body::from(body))
        .unwrap()
}

#[tokio::test]
async fn webhook_dispatches_action() {
    let state = build_test_state();
    submitted_order(&state, "ent-1").await;

    let path_and_query = "/webhooks/test-tenant/oauth/callback?state=ent-1&code=abc123";
    let signature = sign_webhook("GET", path_and_query, b"");
    let request = Request::builder()
        .method("GET")
        .uri(path_and_query)
        .header("x-webhook-signature", signature)
        .body(Body::empty())
        .unwrap();
    let (status, body) = send(state, request).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(json["success"].as_bool().unwrap_or(false), "{body}");
    assert_eq!(json["state"]["status"], "Authorized");
}

#[tokio::test]
async fn stripe_event_body_reaches_the_action() {
    let state = build_test_state();
    submitted_order(&state, "ent-1").await;
    let body = stripe_event("ent-1");
    let signature = stripe_signature(&body, 0);
    let (status, response) = send(state, stripe_post(body, &signature)).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let json: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert!(json["success"].as_bool().unwrap_or(false), "{response}");
    assert_eq!(json["state"]["status"], "Paid");
    let fields = &json["state"]["fields"];
    assert_eq!(fields["amount_total"], 1999, "body numbers stay numbers");
    assert_eq!(fields["event_id"], "evt_1");
    assert_eq!(fields["delivery"], "d-1", "headers are sources too");
}

#[tokio::test]
async fn bodies_over_64_kib_reach_the_action() {
    // The receiver used to cap bodies at 64 KiB with no reason given.
    let state = build_test_state();
    submitted_order(&state, "ent-1").await;
    let body = serde_json::to_vec(&serde_json::json!({
        "id": "evt_1",
        "data": {"object": {"client_reference_id": "ent-1", "amount_total": 1, "padding": "x".repeat(200 * 1024)}}
    }))
    .unwrap();
    assert!(body.len() > 64 * 1024);
    let signature = stripe_signature(&body, 0);
    let (status, response) = send(state, stripe_post(body, &signature)).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    assert!(response.contains("\"Paid\""), "{response}");
}

#[tokio::test]
async fn stale_stripe_signature_is_refused() {
    let state = build_test_state();
    submitted_order(&state, "ent-1").await;
    let body = stripe_event("ent-1");
    let signature = stripe_signature(&body, 301);
    let (status, _) = send(state, stripe_post(body, &signature)).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn signature_is_checked_before_the_body_is_read() {
    let state = build_test_state();
    let unsigned = stripe_post(b"not json".to_vec(), "t=1,v1=00");
    assert_eq!(
        send(state.clone(), unsigned).await.0,
        StatusCode::UNAUTHORIZED
    );

    let signature = stripe_signature(b"not json", 0);
    let (status, response) = send(state, stripe_post(b"not json".to_vec(), &signature)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(response.contains("not JSON"), "{response}");
}

#[tokio::test]
async fn missing_body_entity_id_returns_400() {
    let state = build_test_state();
    let body = serde_json::to_vec(
        &serde_json::json!({"id": "evt_1", "data": {"object": {"client_reference_id": null}}}),
    )
    .unwrap();
    let signature = stripe_signature(&body, 0);
    let (status, response) = send(state, stripe_post(body, &signature)).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        response.contains("body.data.object.client_reference_id"),
        "{response}"
    );
}

#[tokio::test]
async fn any_method_reaches_a_webhook_that_declares_none() {
    // Only GET and POST used to be routed.
    let state = build_test_state();
    submitted_order(&state, "42").await;
    let body = br#"{"action":"opened","issue":{"number":42}}"#;
    let signature = format!(
        "sha256={}",
        hex_encode(&hmac_sha256(GITHUB_SECRET.as_bytes(), body))
    );
    let request = Request::builder()
        .method("PUT")
        .uri("/webhooks/test-tenant/github/issue")
        .header("X-Hub-Signature-256", signature)
        .header("X-GitHub-Delivery", "72d3162e")
        .body(Body::from(body.to_vec()))
        .unwrap();
    let (status, response) = send(state, request).await;
    assert_eq!(status, StatusCode::OK, "{response}");
    let json: serde_json::Value = serde_json::from_str(&response).unwrap();
    assert_eq!(json["state"]["entity_id"], "42", "a number id becomes text");
    assert_eq!(json["state"]["status"], "Paid");
    assert_eq!(json["state"]["fields"]["delivery"], "72d3162e");
}

#[tokio::test]
async fn a_declared_method_is_kept() {
    let state = build_test_state();
    submitted_order(&state, "ent-1").await;
    let path_and_query = "/webhooks/test-tenant/oauth/callback?state=ent-1&code=abc123";
    let request = Request::builder()
        .method("POST")
        .uri(path_and_query)
        .header(
            "x-webhook-signature",
            sign_webhook("POST", path_and_query, b""),
        )
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(state, request).await.0, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn webhook_bad_hmac_returns_401() {
    let state = build_test_state();
    submitted_order(&state, "ent-1").await;
    let request = Request::builder()
        .method("GET")
        .uri("/webhooks/test-tenant/oauth/callback?state=ent-1&code=abc123")
        .header("x-webhook-signature", "deadbeef")
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(state, request).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn webhook_valid_hmac_without_permit_returns_403() {
    let state = build_test_state();
    state
        .authz
        .reload_tenant_policies("test-tenant", "")
        .expect("clear webhook permit");
    submitted_order(&state, "ent-1").await;
    let path_and_query = "/webhooks/test-tenant/oauth/callback?state=ent-1&code=abc123";
    let request = Request::builder()
        .method("GET")
        .uri(path_and_query)
        .header(
            "x-webhook-signature",
            sign_webhook("GET", path_and_query, b""),
        )
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(state, request).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn webhook_missing_entity_id_returns_400() {
    let state = build_test_state();
    let path_and_query = "/webhooks/test-tenant/oauth/callback?code=abc123";
    let request = Request::builder()
        .method("GET")
        .uri(path_and_query)
        .header(
            "x-webhook-signature",
            sign_webhook("GET", path_and_query, b""),
        )
        .body(Body::empty())
        .unwrap();
    assert_eq!(send(state, request).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn webhook_unknown_path_returns_404() {
    let request = Request::builder()
        .method("GET")
        .uri("/webhooks/test-tenant/nonexistent/path?entity_id=ent-1")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(build_test_state(), request).await.0,
        StatusCode::NOT_FOUND
    );
}

#[test]
fn install_refuses_webhooks_that_could_never_fire() {
    // Both used to install: the first answered every call 401, the second
    // never sent its request.
    let unsigned = ORDER_IOA.replace("hmac_secret = \"{secret:STRIPE_WEBHOOK_SECRET}\"\n", "");
    let outbound = ORDER_IOA.replace(
        "params = [\"code\"]\n",
        "params = [\"code\"]\n\n[[action.triggers]]\nname = \"notify\"\nkind = \"webhook\"\nurl = \"https://example.com/hook\"\nmethod = \"POST\"\n",
    );
    for (spec, needle) in [
        (unsigned, "hmac_secret is required"),
        (outbound, "kind = \"webhook\""),
    ] {
        let mut registry = crate::registry::SpecRegistry::new();
        let error = registry
            .try_register_tenant(
                "t",
                parse_csdl(CSDL_XML).unwrap(),
                CSDL_XML.to_string(),
                &[("Order", spec.as_str())],
            )
            .expect_err("install must refuse it");
        assert!(error.to_string().contains(needle), "{error}");
    }
}

#[test]
fn sources_read_query_headers_and_body_paths() {
    let query: BTreeMap<String, String> = [("code".to_string(), "auth-code-123".to_string())]
        .into_iter()
        .collect();
    let mut headers = HeaderMap::new();
    headers.insert("x-delivery", "d-1".parse().unwrap());
    let body = serde_json::json!({
        "data": {"object": {"id": "cs_1", "amount": 1999, "paid": true, "note": null}},
        "items": [{"sku": "a"}, {"sku": "b"}]
    });
    let read = |source: &str| read_source(source, &query, &headers, Some(&body));
    assert_eq!(read("query.code"), Some(Value::from("auth-code-123")));
    assert_eq!(read("header.X-Delivery"), Some(Value::from("d-1")));
    assert_eq!(read("body.data.object.id"), Some(Value::from("cs_1")));
    assert_eq!(read("body.data.object.amount"), Some(Value::from(1999)));
    assert_eq!(read("body.data.object.paid"), Some(Value::from(true)));
    assert_eq!(read("body.items.1.sku"), Some(Value::from("b")));
    assert_eq!(read("body"), Some(body.clone()));
    assert_eq!(read("body.data.object.note"), None, "null counts as absent");
    assert_eq!(read("body.items.9.sku"), None);
    assert_eq!(read("body.data.object.id.deeper"), None);
    assert_eq!(read("query.missing"), None);
    assert_eq!(read("header.x-missing"), None);
}
