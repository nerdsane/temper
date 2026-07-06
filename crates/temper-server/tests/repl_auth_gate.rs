//! `/api/repl` authorization gate (ARN-166).
//!
//! `POST /api/repl` executes agent-supplied code that can drive every non-host
//! `temper.*` method, so executing it is a privileged capability gated behind
//! Cedar (`execute_repl` on `Sandbox`). These tests pin the gate against a real,
//! deny-by-default tenant policy set:
//!
//! - an agent with no `execute_repl` grant is denied (403) before any code runs;
//! - an admin bypasses the gate (matching `require_policy_auth`) and runs;
//! - an agent that IS granted `execute_repl` runs — proving the gate is a real
//!   Cedar evaluation keyed on the action, not a blanket agent-deny.
//!
//! The route is mounted only under the `observe` feature — the same feature
//! production (`temper-cli`) builds with — so this file compiles to nothing
//! without it. Run with: `cargo test -p temper-server --test repl_auth_gate
//! --features observe`.
#![cfg(feature = "observe")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_server::{ServerState, SpecRegistry, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

/// Grants `execute_repl` to admins only — an agent is not covered, so Cedar
/// default-denies it. (Admins bypass the gate before Cedar anyway; the grant
/// keeps the policy realistic rather than empty.)
const ADMIN_ONLY_REPL: &str =
    r#"permit(principal is Admin, action == Action::"execute_repl", resource);"#;

/// Grants `execute_repl` to agents — the authorized-agent surface.
const AGENT_REPL: &str =
    r#"permit(principal is Agent, action == Action::"execute_repl", resource);"#;

/// Build server state whose `default` tenant has `policy` loaded, so the gate is
/// evaluated against real Cedar policies rather than the permissive fallback
/// that `from_registry` installs (which would allow everything and make the
/// deny assertion vacuous).
fn state_with_policy(policy: &str) -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    let state = ServerState::from_registry(ActorSystem::new("repl-auth-gate-test"), registry);
    state
        .authz
        .reload_tenant_policies("default", policy)
        .expect("tenant policy should load");
    state
}

fn repl_request(principal_kind: &str, code: &str) -> Request<Body> {
    Request::post("/api/repl")
        .header("Content-Type", "application/json")
        .header("X-Temper-Principal-Kind", principal_kind)
        .header("X-Temper-Principal-Id", "arn166-test-principal")
        .body(Body::from(serde_json::json!({ "code": code }).to_string()))
        .unwrap()
}

/// An agent with no `execute_repl` grant is denied with 403 before any code
/// runs — the tenant policy grants the action to admins only, so Cedar
/// default-denies the agent.
#[tokio::test]
async fn repl_denied_for_unauthorized_agent() {
    let app = build_router(state_with_policy(ADMIN_ONLY_REPL));
    let response = app
        .oneshot(repl_request("agent", "result = 1 + 1"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"]["code"], "AuthorizationDenied");
}

/// Admin bypasses the gate (matching `require_policy_auth`) even against the
/// deny-by-default policy set, so the REPL still runs for an authorized surface
/// — the gate closes the hole without removing the capability. A clean run
/// returns 200 with `error: null`; a sandbox execution error would surface as a
/// non-null `error` at 200 (see `handle_repl`), so asserting both status and a
/// null `error` pins that the code actually executed rather than silently
/// erroring.
#[tokio::test]
async fn repl_allowed_for_admin() {
    let app = build_router(state_with_policy(ADMIN_ONLY_REPL));
    let response = app
        .oneshot(repl_request("admin", "result = 1 + 1"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], serde_json::Value::Null);
}

/// An agent that IS granted `execute_repl` passes the gate and runs — proving
/// the gate is a real Cedar evaluation keyed on the action, not a hardcoded
/// agent-deny. Same success shape as the admin case: 200 with `error: null`.
#[tokio::test]
async fn repl_allowed_for_authorized_agent() {
    let app = build_router(state_with_policy(AGENT_REPL));
    let response = app
        .oneshot(repl_request("agent", "result = 1 + 1"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["error"], serde_json::Value::Null);
}
