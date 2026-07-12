use super::*;
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

[[webhook]]
name = "pay_callback"
path = "pay/callback"
method = "POST"
action = "HandleOAuthCallback"
entity_lookup = "query_param"
entity_param = "state"
hmac_secret = "{secret:WEBHOOK_SECRET}"
hmac_header = "X-Temper-Signature"

[webhook.extract]
code = "query.code"

[[webhook]]
name = "bad_callback"
path = "bad/callback"
method = "POST"
action = "HandleOAuthCallback"
entity_lookup = "query_param"
entity_param = "state"
hmac_secret = "{secret:MISSING_SECRET}"
hmac_header = "X-Temper-Signature"
"#;

/// Signing secret provisioned into the test tenant's vault for the
/// `pay_callback` signed webhook.
const TEST_WEBHOOK_SECRET: &str = "whsec_test_123";

/// Compute a GitHub-style `sha256=<hex>` HMAC signature over `body`.
fn sign_webhook(secret: &str, body: &[u8]) -> String {
    use hmac::{Hmac, Mac};
    use sha2::Sha256;
    let mut mac =
        Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC accepts any key length");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn build_test_state() -> ServerState {
    let csdl = parse_csdl(CSDL_XML).unwrap();
    let system = ActorSystem::new("webhook-test");
    let vault = crate::secrets::vault::SecretsVault::new(&[0x11u8; 32]);
    vault
        .cache_secret(
            "test-tenant",
            "WEBHOOK_SECRET",
            TEST_WEBHOOK_SECRET.to_string(),
        )
        .expect("cache webhook secret");
    let state = ServerState::new(system, csdl, CSDL_XML.to_string()).with_secrets_vault(vault);

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

/// Create `ent-1` and move it to `Submitted` so the webhook's
/// `HandleOAuthCallback` (from `Submitted`) is a valid transition.
async fn seed_submitted_order(state: &ServerState) {
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
}

fn build_test_router() -> axum::Router {
    crate::router::build_router(build_test_state())
}

#[tokio::test]
async fn webhook_dispatches_action() {
    let state = build_test_state();
    let tenant = TenantId::new("test-tenant");

    // Create entity directly via dispatch.
    let _create = state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            "ent-1",
            serde_json::json!({"id": "ent-1"}),
        )
        .await
        .expect("entity creation should succeed");

    // Submit to move to "Submitted".
    let submit = state
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
    assert!(submit.success, "SubmitOrder should succeed");
    assert_eq!(submit.state.status, "Submitted");

    // Build router and call webhook.
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

// ── Class B: authenticated webhook ingress (ARN-171) ──────────────────────

/// Exploit test: a POST to a signed webhook with NO signature header must
/// be rejected. Before ARN-171 this dispatched the action unauthenticated.
#[tokio::test]
async fn webhook_rejects_missing_signature() {
    let app = crate::router::build_router(build_test_state());
    let body = br#"{"event":"paid"}"#;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/test-tenant/pay/callback?state=ent-1&code=abc123")
                .body(Body::from(body.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// Exploit test: a POST to a signed webhook with a WRONG signature must be
/// rejected. Before ARN-171 the signature was never computed or compared.
#[tokio::test]
async fn webhook_rejects_invalid_signature() {
    let app = crate::router::build_router(build_test_state());
    let body = br#"{"event":"paid"}"#;
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/test-tenant/pay/callback?state=ent-1&code=abc123")
                .header("X-Temper-Signature", "sha256=deadbeefdeadbeef")
                .body(Body::from(body.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// A correctly-signed request whose webhook principal the tenant's Cedar
/// policy does NOT permit must be denied (403). Before ARN-171 the webhook
/// path never touched Cedar, so a denied principal succeeded.
#[tokio::test]
async fn webhook_valid_signature_but_cedar_denies_returns_403() {
    let state = build_test_state();
    seed_submitted_order(&state).await;
    // Tenant policy set that permits a DIFFERENT action — switches the
    // tenant to its own (default-deny) policy set, so the webhook action
    // (HandleOAuthCallback) is not permitted.
    state
        .authz
        .reload_tenant_policies(
            "test-tenant",
            r#"permit(principal, action == Action::"SubmitOrder", resource is Order);"#,
        )
        .expect("install Cedar policy");

    let body = br#"{"event":"paid"}"#;
    let sig = sign_webhook(TEST_WEBHOOK_SECRET, body);
    let app = crate::router::build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/test-tenant/pay/callback?state=ent-1&code=abc123")
                .header("X-Temper-Signature", sig)
                .body(Body::from(body.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// A correctly-signed AND Cedar-authorized request still dispatches the
/// action and transitions the entity.
#[tokio::test]
async fn webhook_valid_signature_and_authorized_succeeds() {
    let state = build_test_state();
    seed_submitted_order(&state).await;
    state
        .authz
        .reload_tenant_policies(
            "test-tenant",
            r#"permit(principal, action == Action::"HandleOAuthCallback", resource is Order);"#,
        )
        .expect("install Cedar policy");

    let body = br#"{"event":"paid"}"#;
    let sig = sign_webhook(TEST_WEBHOOK_SECRET, body);
    let app = crate::router::build_router(state);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/test-tenant/pay/callback?state=ent-1&code=abc123")
                .header("X-Temper-Signature", sig)
                .body(Body::from(body.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["state"]["status"], "Authorized");
}

/// A replay of the exact same signed delivery must be idempotent. Without the
/// route-derived idempotency key, the second request tries to apply
/// `HandleOAuthCallback` from `Authorized` and fails instead of replaying the
/// first successful result.
#[tokio::test]
async fn webhook_replay_returns_original_success_once() {
    let state = build_test_state();
    seed_submitted_order(&state).await;
    state
        .authz
        .reload_tenant_policies(
            "test-tenant",
            r#"permit(principal, action == Action::"HandleOAuthCallback", resource is Order);"#,
        )
        .expect("install Cedar policy");

    let body = br#"{"event":"paid"}"#;
    let sig = sign_webhook(TEST_WEBHOOK_SECRET, body);
    let uri = "/webhooks/test-tenant/pay/callback?state=ent-1&code=abc123";
    let app = crate::router::build_router(state);

    for attempt in 1..=2 {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("X-Temper-Signature", sig.clone())
                    .body(Body::from(body.to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(
            response.status(),
            StatusCode::OK,
            "webhook replay attempt {attempt} should return the original success"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["state"]["status"], "Authorized");
        assert!(
            json["success"].as_bool().unwrap_or(false),
            "webhook replay attempt {attempt} should stay successful"
        );
    }
}

/// A well-formed but WRONG 64-hex signature must be rejected. Unlike the
/// short `deadbeef` case (rejected on length), this exercises the
/// equal-length constant-time byte comparison.
#[tokio::test]
async fn webhook_rejects_full_length_wrong_signature() {
    let app = crate::router::build_router(build_test_state());
    let body = br#"{"event":"paid"}"#;
    let wrong = format!("sha256={}", "a".repeat(64));
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/test-tenant/pay/callback?state=ent-1&code=abc123")
                .header("X-Temper-Signature", wrong)
                .body(Body::from(body.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// A webhook that declares `hmac_secret` referencing an unresolvable
/// `{secret:...}` must fail closed (401) — the signing secret is not
/// configured, so no request can be authenticated.
#[tokio::test]
async fn webhook_unresolvable_secret_returns_401() {
    let app = crate::router::build_router(build_test_state());
    let body = br#"{"event":"paid"}"#;
    // Any signature — the secret can't be resolved, so it must be rejected
    // before comparison.
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/test-tenant/bad/callback?state=ent-1&code=abc123")
                .header("X-Temper-Signature", "sha256=whatever")
                .body(Body::from(body.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// A webhook with NO declared secret is still governed by Cedar: in a
/// tenant with a deny-by-default policy set that does not permit the
/// webhook action, the (unauthenticated) call is denied 403 — proving the
/// authorization gate runs independently of the signature gate.
#[tokio::test]
async fn webhook_no_secret_cedar_denied_returns_403() {
    let state = build_test_state();
    seed_submitted_order(&state).await;
    // Policy set permits a different action → default-deny for the
    // webhook's HandleOAuthCallback.
    state
        .authz
        .reload_tenant_policies(
            "test-tenant",
            r#"permit(principal, action == Action::"SubmitOrder", resource is Order);"#,
        )
        .expect("install Cedar policy");

    // oauth_callback (GET) declares no hmac_secret — no signature required.
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

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}
