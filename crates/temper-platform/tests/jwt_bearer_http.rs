//! A bearer JWT gets a caller in only when a registered issuer signed it, for
//! this kernel's audience, and it is inside its time window.
//!
//! Every request goes through the platform router with a real
//! `Authorization: Bearer <jwt>` header, the way an agent calls Temper. The
//! tokens are real ES256 JWTs and the issuers are real `TrustedIssuer` records.
//! What the caller sees is the answer: 200 when the token is good, 401 when it
//! is not. Each test reports every case that went the wrong way, so one broken
//! check is named by the case that catches it.

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{Value, json};
use temper_platform::{PlatformState, bootstrap_agent_specs, bootstrap_system_tenant};
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_server::request_context::AgentContext;
use tower::ServiceExt;

const TENANT: &str = "jwt-bearer-http";
/// The issuer most tokens here come from. It holds one signing key, `a1`.
const ISSUER_A: &str = "https://issuer-a.jwt-bearer-http.local";
/// A second issuer holding two keys, `b1` and `b2`, so its tokens must say which.
const ISSUER_B: &str = "https://issuer-b.jwt-bearer-http.local";
/// The audience both issuers are registered with: this kernel.
const AUDIENCE: &str = "temper-jwt-bearer-http";
/// The agent every token names. The policy lets it list agent types.
const AGENT: &str = "kc_agent_jwt_bearer_http";

const POLICY: &str = r#"
permit(
  principal == Agent::"kc_agent_jwt_bearer_http",
  action == Action::"list",
  resource is AgentType
);
"#;

fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_slice(&[seed; 32]).expect("a fixed non-zero scalar is a valid P-256 key")
}

fn key_a1() -> SigningKey {
    signing_key(7)
}

fn key_b1() -> SigningKey {
    signing_key(11)
}

fn key_b2() -> SigningKey {
    signing_key(13)
}

/// A key no registered issuer holds.
fn rogue_key() -> SigningKey {
    signing_key(9)
}

fn jwk(key: &SigningKey, kid: &str) -> Value {
    let point = key.verifying_key().to_encoded_point(false);
    json!({
        "kty": "EC",
        "crv": "P-256",
        "kid": kid,
        "x": URL_SAFE_NO_PAD.encode(point.x().expect("an uncompressed point has x")),
        "y": URL_SAFE_NO_PAD.encode(point.y().expect("an uncompressed point has y")),
    })
}

fn b64(value: &Value) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).expect("JSON serializes"))
}

/// A compact JWS: header and claims as given, ES256-signed with `key`.
fn sign(key: &SigningKey, header: &Value, claims: &Value) -> String {
    let signing_input = format!("{}.{}", b64(header), b64(claims));
    let signature: Signature = key.sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

fn header(kid: &str) -> Value {
    json!({ "alg": "ES256", "kid": kid, "typ": "JWT" })
}

/// The claims of a good token from `issuer`: valid from a minute ago for an hour.
fn claims(issuer: &str) -> Value {
    let now = sim_now().timestamp();
    json!({
        "iss": issuer,
        "sub": "human-jwt-bearer-http",
        "aud": AUDIENCE,
        "client_id": AGENT,
        "agent_type": "contributor",
        "nbf": now - 60,
        "exp": now + 3600,
    })
}

/// `value` with one field set.
fn with(mut value: Value, field: &str, to: Value) -> Value {
    value[field] = to;
    value
}

/// `value` with one field removed.
fn without(mut value: Value, field: &str) -> Value {
    value
        .as_object_mut()
        .expect("claims are an object")
        .remove(field);
    value
}

fn now() -> i64 {
    sim_now().timestamp()
}

async fn register_issuer(state: &PlatformState, issuer: &str, keys: Value) {
    let response = state
        .server
        .dispatch_tenant_action(
            &TenantId::new(TENANT),
            "TrustedIssuer",
            issuer,
            "RegisterIssuer",
            json!({
                "issuer": issuer,
                "jwks_json": json!({ "keys": keys }).to_string(),
                "audience": AUDIENCE,
                "algorithms": "ES256",
                "description": "bearer JWT HTTP test issuer",
                "created_by": "jwt_bearer_http",
            }),
            &AgentContext::system(),
        )
        .await
        .expect("RegisterIssuer dispatches");
    assert!(
        response.success,
        "RegisterIssuer {issuer}: {:?}",
        response.error
    );
}

/// The platform router over a tenant with both issuers registered.
async fn platform_router() -> axum::Router {
    let state = PlatformState::new(None);
    bootstrap_system_tenant(&state, &BTreeMap::new());
    bootstrap_agent_specs(&state, TENANT, false, &BTreeMap::new());
    register_issuer(&state, ISSUER_A, json!([jwk(&key_a1(), "a1")])).await;
    register_issuer(
        &state,
        ISSUER_B,
        json!([jwk(&key_b1(), "b1"), jwk(&key_b2(), "b2")]),
    )
    .await;
    state
        .server
        .authz
        .reload_tenant_policies(TENANT, POLICY)
        .expect("the test policy parses");
    temper_platform::router::build_platform_router(state)
}

/// The status an agent sees listing agent types with `token` as its bearer.
async fn status_with(app: &axum::Router, token: &str) -> StatusCode {
    app.clone()
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", format!("Bearer {token}"))
                .header("X-Tenant-Id", TENANT)
                .body(Body::empty())
                .expect("the request builds"),
        )
        .await
        .expect("the router answers")
        .status()
}

/// Each case whose status is not `expected`, with the status it got.
async fn cases_answered_otherwise(
    app: &axum::Router,
    cases: &[(&str, String)],
    expected: StatusCode,
) -> Vec<String> {
    let mut wrong = Vec::new();
    for (name, token) in cases {
        let status = status_with(app, token).await;
        if status != expected {
            wrong.push(format!("{name}: {status}"));
        }
    }
    wrong
}

#[tokio::test]
async fn a_token_a_registered_issuer_signed_for_this_kernel_gets_in() {
    let app = platform_router().await;
    let now = now();

    let cases = [
        (
            "a plain token from issuer A",
            sign(&key_a1(), &header("a1"), &claims(ISSUER_A)),
        ),
        (
            "an audience list that includes this kernel",
            sign(
                &key_a1(),
                &header("a1"),
                &with(
                    claims(ISSUER_A),
                    "aud",
                    json!(["another-service", AUDIENCE]),
                ),
            ),
        ),
        (
            "expired 30 s ago, inside the clock-skew allowance",
            sign(
                &key_a1(),
                &header("a1"),
                &with(
                    with(claims(ISSUER_A), "nbf", json!(now - 3600)),
                    "exp",
                    json!(now - 30),
                ),
            ),
        ),
        (
            "valid in 30 s, inside the clock-skew allowance",
            sign(
                &key_a1(),
                &header("a1"),
                &with(claims(ISSUER_A), "nbf", json!(now + 30)),
            ),
        ),
        (
            "issuer B's second key, named by its kid",
            sign(&key_b2(), &header("b2"), &claims(ISSUER_B)),
        ),
    ];

    let refused = cases_answered_otherwise(&app, &cases, StatusCode::OK).await;
    assert!(
        refused.is_empty(),
        "good tokens must get in; these did not: {refused:#?}"
    );
}

#[tokio::test]
async fn a_token_not_signed_for_this_kernel_now_is_refused() {
    let app = platform_router().await;
    let now = now();

    // The same request with a good token gets in, so each 401 below is the
    // token's doing and not the setup's.
    let good = sign(&key_a1(), &header("a1"), &claims(ISSUER_A));
    assert_eq!(status_with(&app, &good).await, StatusCode::OK);

    let good_parts: Vec<&str> = good.split('.').collect();
    let forged_payload = b64(&with(
        claims(ISSUER_A),
        "client_id",
        json!("kc_someone_else"),
    ));

    let cases = [
        (
            "audience is another service",
            sign(
                &key_a1(),
                &header("a1"),
                &with(claims(ISSUER_A), "aud", json!("another-service")),
            ),
        ),
        (
            "audience list without this kernel",
            sign(
                &key_a1(),
                &header("a1"),
                &with(
                    claims(ISSUER_A),
                    "aud",
                    json!(["one-service", "two-service"]),
                ),
            ),
        ),
        (
            "not valid for another hour",
            sign(
                &key_a1(),
                &header("a1"),
                &with(claims(ISSUER_A), "nbf", json!(now + 3600)),
            ),
        ),
        (
            "expired an hour ago",
            sign(
                &key_a1(),
                &header("a1"),
                &with(
                    with(claims(ISSUER_A), "nbf", json!(now - 7200)),
                    "exp",
                    json!(now - 3600),
                ),
            ),
        ),
        (
            "no expiry at all",
            sign(&key_a1(), &header("a1"), &without(claims(ISSUER_A), "exp")),
        ),
        (
            "an issuer nobody registered",
            sign(
                &key_a1(),
                &header("a1"),
                &claims("https://unregistered.jwt-bearer-http.local"),
            ),
        ),
        (
            "claims issuer B, signed with issuer A's key",
            sign(&key_a1(), &header("b1"), &claims(ISSUER_B)),
        ),
        (
            "signed by a key no issuer holds",
            sign(&rogue_key(), &header("a1"), &claims(ISSUER_A)),
        ),
        (
            "payload changed after signing",
            format!("{}.{forged_payload}.{}", good_parts[0], good_parts[2]),
        ),
        (
            "header says alg none",
            sign(
                &key_a1(),
                &json!({ "alg": "none", "kid": "a1", "typ": "JWT" }),
                &claims(ISSUER_A),
            ),
        ),
        (
            "header says alg HS256",
            sign(
                &key_a1(),
                &json!({ "alg": "HS256", "kid": "a1", "typ": "JWT" }),
                &claims(ISSUER_A),
            ),
        ),
        (
            "names a kid issuer A does not hold",
            sign(&key_a1(), &header("a-unknown"), &claims(ISSUER_A)),
        ),
        (
            "names no kid, and issuer B holds two keys",
            sign(
                &key_b1(),
                &json!({ "alg": "ES256", "typ": "JWT" }),
                &claims(ISSUER_B),
            ),
        ),
        (
            "three segments of junk",
            "not-base64!.not-json!.not-a-signature!".to_string(),
        ),
    ];

    let admitted = cases_answered_otherwise(&app, &cases, StatusCode::UNAUTHORIZED).await;
    assert!(
        admitted.is_empty(),
        "bad tokens must be refused with 401; these were not: {admitted:#?}"
    );
}
