//! E2E tests for platform-assigned agent identity (ADR-0033).
//!
//! Exercises the full credential → identity resolution pipeline through
//! real entity actors, real `TransitionTable` evaluation, and real
//! `IdentityResolver` lookups. Proves:
//!
//! 1. AgentType + AgentCredential lifecycle works end-to-end
//! 2. Identity resolver correctly maps bearer tokens to verified identities
//! 3. Credential rotation/revocation invalidates resolution
//! 4. Deprecated AgentType blocks credential resolution
//! 5. Bearer auth middleware resolves agent credentials and exempts `/api/identity/resolve`

use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::collections::BTreeMap;
use temper_platform::bootstrap::{bootstrap_agent_specs, bootstrap_system_tenant};
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::identity::{IdentityResolver, hash_token};
use temper_server::request_context::AgentContext;
use tower::ServiceExt;

mod common;

use common::http::body_json;

const TEST_TENANT: &str = "identity-test";

/// Build a `PlatformState` with both system and agent specs bootstrapped
/// on a dedicated test tenant.
fn identity_test_state() -> PlatformState {
    let state = PlatformState::new(None);
    bootstrap_system_tenant(&state, &BTreeMap::new());
    bootstrap_agent_specs(&state, TEST_TENANT, false, &BTreeMap::new());
    state
}

/// Helper: dispatch a tenant action and return the response.
async fn dispatch(
    state: &PlatformState,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    params: serde_json::Value,
) -> temper_server::entity_actor::EntityResponse {
    state
        .server
        .dispatch_tenant_action(
            &TenantId::new(TEST_TENANT),
            entity_type,
            entity_id,
            action,
            params,
            &AgentContext::system(),
        )
        .await
        .unwrap_or_else(|e| panic!("dispatch {entity_type}.{action} failed: {e}"))
}

// =========================================================================
// Dispatch-level identity tests
// =========================================================================

/// Full AgentType lifecycle: Draft → Active → Deprecated → Active.
#[tokio::test]
async fn e2e_agent_type_lifecycle() {
    let state = identity_test_state();

    // Created in Draft
    let r = dispatch(
        &state,
        "AgentType",
        "at-1",
        "Define",
        serde_json::json!({
            "name": "claude-code",
            "system_prompt": "test",
            "tool_set": "local",
            "model": "claude-sonnet-4-6",
            "max_turns": "200",
            "adapter_config": "{}",
            "default_budget_cents": "0"
        }),
    )
    .await;
    assert!(r.success, "Define: {:?}", r.error);
    assert_eq!(r.state.status, "Active");
    assert_eq!(
        r.state.fields.get("name").and_then(|v| v.as_str()),
        Some("claude-code")
    );

    // Deprecate
    let r = dispatch(
        &state,
        "AgentType",
        "at-1",
        "Deprecate",
        serde_json::json!({}),
    )
    .await;
    assert!(r.success, "Deprecate: {:?}", r.error);
    assert_eq!(r.state.status, "Deprecated");

    // Reactivate
    let r = dispatch(
        &state,
        "AgentType",
        "at-1",
        "Reactivate",
        serde_json::json!({}),
    )
    .await;
    assert!(r.success, "Reactivate: {:?}", r.error);
    assert_eq!(r.state.status, "Active");
}

/// Full AgentCredential lifecycle: Issue → Rotate → Revoke.
#[tokio::test]
async fn e2e_agent_credential_lifecycle() {
    let state = identity_test_state();

    let key_hash = hash_token("test-api-key-1");

    // Issue (entity starts Active, Issue is a self-transition)
    let r = dispatch(
        &state,
        "AgentCredential",
        &key_hash,
        "Issue",
        serde_json::json!({
            "agent_type_id": "at-1",
            "agent_instance_id": "inst-1",
            "key_hash": key_hash,
            "key_prefix": "tmpr_test",
            "description": "E2E test credential",
            "created_by": "test",
            "expires_at": ""
        }),
    )
    .await;
    assert!(r.success, "Issue: {:?}", r.error);
    assert_eq!(r.state.status, "Active");

    // Rotate
    let r = dispatch(
        &state,
        "AgentCredential",
        &key_hash,
        "Rotate",
        serde_json::json!({
            "key_hash": "rotated-hash",
            "key_prefix": "tmpr_rota",
            "description": "Rotated"
        }),
    )
    .await;
    assert!(r.success, "Rotate: {:?}", r.error);
    assert_eq!(r.state.status, "Rotated");

    // Revoke
    let r = dispatch(
        &state,
        "AgentCredential",
        &key_hash,
        "Revoke",
        serde_json::json!({}),
    )
    .await;
    assert!(r.success, "Revoke: {:?}", r.error);
    assert_eq!(r.state.status, "Revoked");
}

/// Identity resolver: valid credential → ResolvedIdentity with correct fields.
#[tokio::test]
async fn e2e_identity_resolution_valid_credential() {
    let state = identity_test_state();
    let tenant = TenantId::new(TEST_TENANT);

    // 1. Create an Active AgentType
    let r = dispatch(
        &state,
        "AgentType",
        "cc-type",
        "Define",
        serde_json::json!({
            "name": "claude-code",
            "system_prompt": "test",
            "tool_set": "local",
            "model": "claude-sonnet-4-6",
            "max_turns": "200",
            "adapter_config": "{}",
            "default_budget_cents": "0"
        }),
    )
    .await;
    assert!(r.success);

    // 2. Issue credential with key_hash as entity ID
    let plaintext_key = "tmpr_e2e-resolution-test";
    let key_hash = hash_token(plaintext_key);

    let r = dispatch(
        &state,
        "AgentCredential",
        &key_hash,
        "Issue",
        serde_json::json!({
            "agent_type_id": "cc-type",
            "agent_instance_id": "platform-inst-42",
            "key_hash": key_hash,
            "key_prefix": "tmpr_e2e-",
            "description": "Resolution test",
            "created_by": "test",
            "expires_at": ""
        }),
    )
    .await;
    assert!(r.success);

    // 3. Resolve the token
    let resolver = IdentityResolver::new();
    let identity = resolver
        .resolve(&state.server, &tenant, plaintext_key)
        .await
        .expect("should resolve valid credential");

    assert_eq!(identity.agent_instance_id, "platform-inst-42");
    assert_eq!(identity.agent_type_id, "cc-type");
    assert_eq!(identity.agent_type_name, "claude-code");
    assert!(identity.verified);
}

/// Identity resolver: invalid token → None.
#[tokio::test]
async fn e2e_identity_resolution_invalid_token() {
    let state = identity_test_state();
    let tenant = TenantId::new(TEST_TENANT);

    let resolver = IdentityResolver::new();
    let result = resolver
        .resolve(&state.server, &tenant, "nonexistent-key")
        .await;
    assert!(result.is_none(), "invalid token should not resolve");
}

/// Identity resolver: rotated credential → None.
#[tokio::test]
async fn e2e_identity_resolution_rotated_credential() {
    let state = identity_test_state();
    let tenant = TenantId::new(TEST_TENANT);

    // Create AgentType
    dispatch(
        &state,
        "AgentType",
        "rot-type",
        "Define",
        serde_json::json!({
            "name": "test-agent",
            "system_prompt": "",
            "tool_set": "local",
            "model": "claude-sonnet-4-6",
            "max_turns": "50",
            "adapter_config": "{}",
            "default_budget_cents": "0"
        }),
    )
    .await;

    // Issue credential
    let plaintext = "tmpr_rotation-test-key";
    let key_hash = hash_token(plaintext);
    dispatch(
        &state,
        "AgentCredential",
        &key_hash,
        "Issue",
        serde_json::json!({
            "agent_type_id": "rot-type",
            "agent_instance_id": "rot-inst",
            "key_hash": key_hash,
            "key_prefix": "tmpr_rota",
            "description": "rotation test",
            "created_by": "test",
            "expires_at": ""
        }),
    )
    .await;

    // Verify it resolves before rotation
    let resolver = IdentityResolver::new();
    assert!(
        resolver
            .resolve(&state.server, &tenant, plaintext)
            .await
            .is_some()
    );

    // Rotate the credential
    dispatch(
        &state,
        "AgentCredential",
        &key_hash,
        "Rotate",
        serde_json::json!({
            "key_hash": "new-hash",
            "key_prefix": "tmpr_new",
            "description": "rotated"
        }),
    )
    .await;

    // Should no longer resolve (status is Rotated, not Active)
    let resolver2 = IdentityResolver::new();
    let result = resolver2.resolve(&state.server, &tenant, plaintext).await;
    assert!(result.is_none(), "rotated credential should not resolve");
}

/// Identity resolver: deprecated AgentType → None.
#[tokio::test]
async fn e2e_identity_resolution_deprecated_agent_type() {
    let state = identity_test_state();
    let tenant = TenantId::new(TEST_TENANT);

    // Create and activate AgentType
    dispatch(
        &state,
        "AgentType",
        "depr-type",
        "Define",
        serde_json::json!({
            "name": "deprecated-agent",
            "system_prompt": "",
            "tool_set": "local",
            "model": "claude-sonnet-4-6",
            "max_turns": "50",
            "adapter_config": "{}",
            "default_budget_cents": "0"
        }),
    )
    .await;

    // Issue credential
    let plaintext = "tmpr_deprecation-test";
    let key_hash = hash_token(plaintext);
    dispatch(
        &state,
        "AgentCredential",
        &key_hash,
        "Issue",
        serde_json::json!({
            "agent_type_id": "depr-type",
            "agent_instance_id": "depr-inst",
            "key_hash": key_hash,
            "key_prefix": "tmpr_depr",
            "description": "deprecation test",
            "created_by": "test",
            "expires_at": ""
        }),
    )
    .await;

    // Resolves before deprecation
    let resolver = IdentityResolver::new();
    assert!(
        resolver
            .resolve(&state.server, &tenant, plaintext)
            .await
            .is_some()
    );

    // Deprecate the AgentType
    dispatch(
        &state,
        "AgentType",
        "depr-type",
        "Deprecate",
        serde_json::json!({}),
    )
    .await;

    // Should no longer resolve (AgentType status is Deprecated, not Active)
    let resolver2 = IdentityResolver::new();
    let result = resolver2.resolve(&state.server, &tenant, plaintext).await;
    assert!(
        result.is_none(),
        "credential linked to deprecated AgentType should not resolve"
    );
}

// =========================================================================
// HTTP-level identity tests
// =========================================================================

/// Build a router with agent specs and an API key configured for bearer auth.
fn identity_test_router() -> axum::Router {
    let mut state = identity_test_state();
    state.api_token = Some("admin-test-key".to_string());
    temper_platform::router::build_platform_router(state)
}

/// Bearer auth: `/api/identity/resolve` is accessible without Authorization header.
#[tokio::test]
async fn e2e_http_identity_resolve_exempt_from_auth() {
    let app = identity_test_router();

    // POST /api/identity/resolve without any Authorization header — should NOT 401
    let response = app
        .oneshot(
            Request::post("/api/identity/resolve")
                .header("Content-Type", "application/json")
                .header("X-Tenant-Id", TEST_TENANT)
                .body(Body::from(r#"{"bearer_token": "nonexistent-token"}"#))
                .unwrap(),
        )
        .await
        .unwrap();

    // Should be 404 (credential not found), NOT 401 (unauthorized)
    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "identity resolve should bypass auth and return 404 for unknown token, not 401"
    );
}

/// Bearer auth: valid agent credential resolves identity on HTTP requests.
#[tokio::test]
async fn e2e_http_agent_credential_auth() {
    let app = identity_test_router();

    // 1. Create AgentType (as admin)
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/AgentTypes")
                .header("Content-Type", "application/json")
                .header("Authorization", "Bearer admin-test-key")
                .header("X-Temper-Principal-Kind", "admin")
                .header("X-Tenant-Id", TEST_TENANT)
                .body(Body::from(r#"{"id": "http-cc-type"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // Define → Active
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/AgentTypes('http-cc-type')/Temper.Agent.Define")
                .header("Content-Type", "application/json")
                .header("Authorization", "Bearer admin-test-key")
                .header("X-Temper-Principal-Kind", "admin")
                .header("X-Tenant-Id", TEST_TENANT)
                .body(Body::from(
                    r#"{"name": "claude-code", "system_prompt": "test", "tool_set": "local", "model": "claude-sonnet-4-6", "max_turns": "200", "adapter_config": "{}", "default_budget_cents": "0"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "Active");

    // 2. Create AgentCredential with key_hash as entity ID
    let agent_key = "tmpr_http-auth-test-key";
    let key_hash = hash_token(agent_key);

    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/AgentCredentials")
                .header("Content-Type", "application/json")
                .header("Authorization", "Bearer admin-test-key")
                .header("X-Temper-Principal-Kind", "admin")
                .header("X-Tenant-Id", TEST_TENANT)
                .body(Body::from(format!(r#"{{"id": "{key_hash}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);

    // Issue the credential
    let response = app
        .clone()
        .oneshot(
            Request::post(format!(
                "/tdata/AgentCredentials('{key_hash}')/Temper.Agent.Issue"
            ))
            .header("Content-Type", "application/json")
            .header("Authorization", "Bearer admin-test-key")
            .header("X-Temper-Principal-Kind", "admin")
            .header("X-Tenant-Id", TEST_TENANT)
            .body(Body::from(format!(
                r#"{{"agent_type_id": "http-cc-type", "agent_instance_id": "http-inst-1", "key_hash": "{key_hash}", "key_prefix": "tmpr_http", "description": "HTTP auth test", "created_by": "test", "expires_at": ""}}"#
            )))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["status"], "Active");

    // 3. Use the agent credential as Bearer token — should be accepted
    let response = app
        .clone()
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", format!("Bearer {agent_key}"))
                .header("X-Tenant-Id", TEST_TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "agent credential should be accepted as Bearer token"
    );

    // 4. Resolve identity via the endpoint
    let response = app
        .clone()
        .oneshot(
            Request::post("/api/identity/resolve")
                .header("Content-Type", "application/json")
                .header("X-Tenant-Id", TEST_TENANT)
                .body(Body::from(format!(r#"{{"bearer_token": "{agent_key}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = body_json(response).await;
    assert_eq!(json["agent_instance_id"], "http-inst-1");
    assert_eq!(json["agent_type_name"], "claude-code");
    assert_eq!(json["verified"], true);
}

/// Bearer auth: no token → 401, wrong token → 401.
#[tokio::test]
async fn e2e_http_missing_and_wrong_token_rejected() {
    let app = identity_test_router();

    // No auth header
    let response = app
        .clone()
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("X-Tenant-Id", TEST_TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

    // Wrong token
    let response = app
        .clone()
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", "Bearer wrong-key")
                .header("X-Tenant-Id", TEST_TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// Bearer auth: global API key passes as admin.
#[tokio::test]
async fn e2e_http_global_api_key_admin_access() {
    let app = identity_test_router();

    let response = app
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", "Bearer admin-test-key")
                .header("X-Temper-Principal-Kind", "admin")
                .header("X-Tenant-Id", TEST_TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

/// Cache coherence: rotating a credential invalidates resolver cache immediately.
#[tokio::test]
async fn e2e_http_rotate_invalidates_identity_cache() {
    let mut state = identity_test_state();
    state.api_token = Some("admin-test-key".to_string());
    let resolver = state.identity_resolver.clone();
    let app = temper_platform::router::build_platform_router(state.clone());
    let tenant = TenantId::new(TEST_TENANT);

    // Create AgentType + credential.
    let r = dispatch(
        &state,
        "AgentType",
        "cache-type",
        "Define",
        serde_json::json!({
            "name": "cache-agent",
            "system_prompt": "test",
            "tool_set": "local",
            "model": "claude-sonnet-4-6",
            "max_turns": "200",
            "adapter_config": "{}",
            "default_budget_cents": "0"
        }),
    )
    .await;
    assert!(r.success);

    let plaintext = "tmpr_cache-invalidate-test";
    let key_hash = hash_token(plaintext);
    let r = dispatch(
        &state,
        "AgentCredential",
        &key_hash,
        "Issue",
        serde_json::json!({
            "agent_type_id": "cache-type",
            "agent_instance_id": "cache-inst-1",
            "key_hash": key_hash,
            "key_prefix": "tmpr_cach",
            "description": "cache test",
            "created_by": "test",
            "expires_at": ""
        }),
    )
    .await;
    assert!(r.success);

    // Populate cache with a successful resolution.
    assert!(
        resolver
            .resolve(&state.server, &tenant, plaintext)
            .await
            .is_some()
    );

    // Rotate through HTTP route (this should trigger middleware invalidation).
    let response = app
        .oneshot(
            Request::post(format!(
                "/tdata/AgentCredentials('{key_hash}')/Temper.Agent.Rotate"
            ))
            .header("Content-Type", "application/json")
            .header("Authorization", "Bearer admin-test-key")
            .header("X-Temper-Principal-Kind", "admin")
            .header("X-Tenant-Id", TEST_TENANT)
            .body(Body::from(
                r#"{"key_hash":"rotated-hash","key_prefix":"tmpr_rot","description":"rotated"}"#,
            ))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Resolver should not return stale cached identity after rotate.
    assert!(
        resolver
            .resolve(&state.server, &tenant, plaintext)
            .await
            .is_none(),
        "rotated credential must be invalidated in resolver cache immediately"
    );
}
