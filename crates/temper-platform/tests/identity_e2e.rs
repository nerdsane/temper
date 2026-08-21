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
use temper_server::StorageStack;
use temper_server::identity::{IdentityResolver, hash_token};
use temper_server::request_context::AgentContext;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

mod common;

use common::http::body_json;

const TEST_TENANT: &str = "identity-test";
const TEST_OPERATOR_KEY: &str = "registered-operator-test-key";

const HTTP_IDENTITY_POLICY: &str = r#"
permit(
  principal == Agent::"operator",
  action == Action::"create",
  resource is AgentType
);
permit(
  principal == Agent::"operator",
  action == Action::"Define",
  resource is AgentType
);
permit(
  principal == Agent::"operator",
  action == Action::"create",
  resource is AgentCredential
);
permit(
  principal == Agent::"operator",
  action == Action::"Issue",
  resource is AgentCredential
);
permit(
  principal == Agent::"operator",
  action == Action::"Rotate",
  resource is AgentCredential
);
permit(
  principal == Agent::"operator",
  action == Action::"delete",
  resource is AgentCredential
);
permit(
  principal == Agent::"http-inst-1",
  action == Action::"list",
  resource is AgentType
);
"#;

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

async fn define_type_and_issue_credential(
    state: &PlatformState,
    agent_type_id: &str,
    agent_type_name: &str,
    plaintext: &str,
    agent_instance_id: &str,
) -> String {
    let response = dispatch(
        state,
        "AgentType",
        agent_type_id,
        "Define",
        serde_json::json!({
            "name": agent_type_name,
            "system_prompt": "test",
            "tool_set": "local",
            "model": "claude-sonnet-4-6",
            "max_turns": "200",
            "adapter_config": "{}",
            "default_budget_cents": "0"
        }),
    )
    .await;
    assert!(response.success, "Define: {:?}", response.error);

    let key_hash = hash_token(plaintext);
    let response = dispatch(
        state,
        "AgentCredential",
        &key_hash,
        "Issue",
        serde_json::json!({
            "agent_type_id": agent_type_id,
            "agent_instance_id": agent_instance_id,
            "key_hash": key_hash,
            "key_prefix": "tmpr_test",
            "description": "identity E2E credential",
            "created_by": "test",
            "expires_at": ""
        }),
    )
    .await;
    assert!(response.success, "Issue: {:?}", response.error);
    key_hash
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
    let result = resolver.resolve(&state.server, &tenant, plaintext).await;
    assert!(result.is_none(), "rotated credential should not resolve");
}

/// Identity resolver: revocation takes effect for an already-used resolver.
#[tokio::test]
async fn e2e_identity_resolution_revocation_is_immediate() {
    let state = identity_test_state();
    let tenant = TenantId::new(TEST_TENANT);
    let plaintext = "tmpr_immediate-revocation-test";
    let key_hash = define_type_and_issue_credential(
        &state,
        "revoke-type",
        "revocable-agent",
        plaintext,
        "revoke-inst",
    )
    .await;
    let resolver = IdentityResolver::new();

    assert!(
        resolver
            .resolve(&state.server, &tenant, plaintext)
            .await
            .is_some()
    );
    let response = dispatch(
        &state,
        "AgentCredential",
        &key_hash,
        "Revoke",
        serde_json::json!({}),
    )
    .await;
    assert!(response.success, "Revoke: {:?}", response.error);

    assert!(
        resolver
            .resolve(&state.server, &tenant, plaintext)
            .await
            .is_none(),
        "revocation must remove authority on the next resolution"
    );
}

/// Identity resolution rejects registry rows whose stored hash no longer
/// matches the entity ID derived from the presented credential.
#[tokio::test]
async fn e2e_identity_resolution_rejects_mismatched_stored_hash() {
    let state = identity_test_state();
    let tenant = TenantId::new(TEST_TENANT);
    let plaintext = "tmpr_hash-binding-test";
    let key_hash = define_type_and_issue_credential(
        &state,
        "hash-binding-type",
        "hash-binding-agent",
        plaintext,
        "hash-binding-inst",
    )
    .await;
    let resolver = IdentityResolver::new();
    assert!(
        resolver
            .resolve(&state.server, &tenant, plaintext)
            .await
            .is_some()
    );

    state
        .server
        .update_tenant_entity_fields(
            &tenant,
            "AgentCredential",
            &key_hash,
            serde_json::json!({"key_hash": "different-hash"}),
            false,
        )
        .await
        .expect("generic test mutation should succeed");

    assert!(
        resolver
            .resolve(&state.server, &tenant, plaintext)
            .await
            .is_none(),
        "mismatched durable lookup ID and stored key hash must fail closed"
    );
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
    let result = resolver.resolve(&state.server, &tenant, plaintext).await;
    assert!(
        result.is_none(),
        "credential linked to deprecated AgentType should not resolve"
    );
}

// =========================================================================
// HTTP-level identity tests
// =========================================================================

/// Build state with an ordinary tenant operator and exact HTTP test policy.
async fn identity_http_state() -> PlatformState {
    let state = identity_test_state();
    temper_platform::bootstrap_operator_credential(&state, TEST_OPERATOR_KEY, TEST_TENANT).await;
    state
        .server
        .authz
        .reload_tenant_policies(TEST_TENANT, HTTP_IDENTITY_POLICY)
        .expect("HTTP identity test policy should parse");
    state
}

async fn identity_test_router() -> axum::Router {
    temper_platform::router::build_platform_router(identity_http_state().await)
}

/// Bearer auth: `/api/identity/resolve` is accessible without Authorization header.
#[tokio::test]
async fn e2e_http_identity_resolve_exempt_from_auth() {
    let app = identity_test_router().await;

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
    let app = identity_test_router().await;

    // 1. Create AgentType as a registered, tenant-scoped operator.
    let response = app
        .clone()
        .oneshot(
            Request::post("/tdata/AgentTypes")
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {TEST_OPERATOR_KEY}"))
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
                .header("Authorization", format!("Bearer {TEST_OPERATOR_KEY}"))
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
                .header("Authorization", format!("Bearer {TEST_OPERATOR_KEY}"))
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
            .header("Authorization", format!("Bearer {TEST_OPERATOR_KEY}"))
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
    let app = identity_test_router().await;

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

/// Bearer auth: configuring a deployment key does not create runtime Admin authority.
#[tokio::test]
async fn e2e_http_unregistered_deployment_key_has_no_admin_fallback() {
    let mut state = identity_test_state();
    state.api_token = Some("unregistered-deployment-key".to_string());
    let app = temper_platform::router::build_platform_router(state);

    let response = app
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", "Bearer unregistered-deployment-key")
                .header("X-Tenant-Id", TEST_TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

/// A generic OData deletion removes credential authority immediately.
#[tokio::test]
async fn e2e_http_generic_delete_removes_credential_authority() {
    let state = identity_http_state().await;
    let resolver = IdentityResolver::new();
    let app = temper_platform::router::build_platform_router(state.clone());
    let tenant = TenantId::new(TEST_TENANT);

    let plaintext = "tmpr_generic-delete-test";
    let key_hash = define_type_and_issue_credential(
        &state,
        "delete-type",
        "deletable-agent",
        plaintext,
        "delete-inst",
    )
    .await;

    // Establish that this resolver has already returned positive authority.
    assert!(
        resolver
            .resolve(&state.server, &tenant, plaintext)
            .await
            .is_some()
    );

    // Use the generic entity mutation path, not an AgentCredential action.
    let response = app
        .oneshot(
            Request::delete(format!("/tdata/AgentCredentials('{key_hash}')"))
                .header("Authorization", format!("Bearer {TEST_OPERATOR_KEY}"))
                .header("X-Tenant-Id", TEST_TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    assert!(
        resolver
            .resolve(&state.server, &tenant, plaintext)
            .await
            .is_none(),
        "generic deletion must remove authority on the next resolution"
    );
}

/// A mutation on one replica is authoritative for identity checks on another.
#[tokio::test]
async fn e2e_replica_revocation_is_read_from_shared_durable_state() {
    let directory = tempfile::tempdir().expect("create identity replica test directory");
    let database_url = format!("file:{}", directory.path().join("identity.db").display());
    let store = TursoEventStore::new(&database_url, None)
        .await
        .expect("create shared identity store");

    let mut first = identity_test_state();
    let registry = first
        .registry
        .read()
        .expect("identity registry lock should be healthy")
        .clone();
    let mut second = PlatformState::with_registry(registry, None);
    first
        .server
        .set_storage_stack(StorageStack::from_turso(store.clone()));
    second
        .server
        .set_storage_stack(StorageStack::from_turso(store));

    let plaintext = "tmpr_cross-replica-revocation";
    let key_hash = define_type_and_issue_credential(
        &first,
        "replica-type",
        "replica-agent",
        plaintext,
        "replica-inst",
    )
    .await;
    let tenant = TenantId::new(TEST_TENANT);
    let resolver = IdentityResolver::new();
    assert!(
        resolver
            .resolve(&first.server, &tenant, plaintext)
            .await
            .is_some()
    );

    let response = dispatch(
        &second,
        "AgentCredential",
        &key_hash,
        "Revoke",
        serde_json::json!({}),
    )
    .await;
    assert!(response.success, "replica Revoke: {:?}", response.error);

    assert!(
        resolver
            .resolve(&first.server, &tenant, plaintext)
            .await
            .is_none(),
        "the first replica must observe revocation from shared durable state"
    );
}

// =========================================================================
// Bootstrap operator credential tests
// =========================================================================

/// Bootstrap operator credential: global API key resolves as verified identity.
#[tokio::test]
async fn e2e_bootstrap_operator_credential_resolves() {
    let state = identity_test_state();
    let api_key = "tmpr_bootstrap-operator-test-key";
    let tenant = TenantId::new(TEST_TENANT);

    // Bootstrap the operator credential for our test tenant.
    temper_platform::bootstrap_operator_credential(&state, api_key, TEST_TENANT).await;

    // The global API key should now resolve as a verified "operator" identity.
    let resolver = IdentityResolver::new();
    let identity = resolver
        .resolve(&state.server, &tenant, api_key)
        .await
        .expect("bootstrap operator credential should resolve");

    assert_eq!(identity.agent_instance_id, "operator");
    assert_eq!(identity.agent_type_name, "operator");
    assert!(identity.verified);
}

/// Bootstrap operator credential is idempotent — calling twice doesn't error.
#[tokio::test]
async fn e2e_bootstrap_operator_credential_idempotent() {
    let state = identity_test_state();
    let api_key = "tmpr_idempotent-bootstrap-test";
    let tenant = TenantId::new(TEST_TENANT);

    // Call twice — should not panic or error.
    temper_platform::bootstrap_operator_credential(&state, api_key, TEST_TENANT).await;
    temper_platform::bootstrap_operator_credential(&state, api_key, TEST_TENANT).await;

    // Should still resolve correctly.
    let resolver = IdentityResolver::new();
    let identity = resolver
        .resolve(&state.server, &tenant, api_key)
        .await
        .expect("operator credential should resolve after double bootstrap");

    assert_eq!(identity.agent_instance_id, "operator");
    assert!(identity.verified);
}

/// Identity resolution is tenant-scoped.
#[tokio::test]
async fn e2e_identity_resolution_is_tenant_scoped() {
    let state = identity_test_state();
    let api_key = "tmpr_tenant_scoped_cache_test";

    temper_platform::bootstrap_operator_credential(&state, api_key, TEST_TENANT).await;

    let resolver = IdentityResolver::new();
    let first = resolver
        .resolve(&state.server, &TenantId::new(TEST_TENANT), api_key)
        .await
        .expect("credential should resolve in source tenant");
    assert_eq!(first.agent_type_name, "operator");

    let leaked = resolver
        .resolve(&state.server, &TenantId::new("other-tenant"), api_key)
        .await;
    assert!(
        leaked.is_none(),
        "identity authority must not cross tenant boundaries"
    );
}

/// Without credential registration, API key does NOT resolve (no fallback).
#[tokio::test]
async fn e2e_unregistered_api_key_does_not_resolve() {
    let state = identity_test_state();
    let tenant = TenantId::new(TEST_TENANT);

    // A key that was never registered should not resolve.
    let resolver = IdentityResolver::new();
    let result = resolver
        .resolve(&state.server, &tenant, "tmpr_never-registered")
        .await;
    assert!(
        result.is_none(),
        "unregistered API key should not resolve to any identity"
    );
}
