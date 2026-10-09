//! #529 — identity dependency-error fail-closed coverage.
//!
//! When the authoritative identity store/replay is unavailable or corrupt,
//! authentication must fail closed as a typed dependency failure mapped to
//! HTTP 503 — never as 401 (invalid credential), 404 (absent record), nor
//! silently as an anonymous/protocol-forwarded request. Genuinely missing,
//! revoked, expired, malformed, or wrong-tenant credentials must remain
//! denied exactly as before (401/403/404) — including a tenant with no
//! identity governance installed at all, which is a configuration state,
//! not an outage (see `ungoverned_tenant_falls_through_to_401_not_503`).
//!
//! All fixtures are in-memory/Sim (`temper_store_sim::SimEventStore`); no
//! `DATABASE_URL`, no Postgres, no Docker, no testcontainers.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::collections::BTreeMap;
use temper_platform::bootstrap::{bootstrap_agent_specs, bootstrap_system_tenant};
use temper_platform::state::PlatformState;
use temper_runtime::tenant::TenantId;
use temper_server::StorageStack;
use temper_server::identity::hash_token;
use temper_server::request_context::AgentContext;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

mod common;
use common::http::body_json;

const TENANT: &str = "identity-dependency-test";
/// A second, independently governed tenant (also bootstrapped with agent
/// specs) used only to prove cross-tenant denial stays 401 — distinct from
/// a tenant that was never bootstrapped at all (see
/// `ungoverned_tenant_falls_through_to_401_not_503`).
const OTHER_GOVERNED_TENANT: &str = "identity-dependency-test-other-tenant";

const POLICY: &str = r#"
permit(principal == Agent::"dep-inst", action == Action::"list", resource is AgentType);
permit(principal == Agent::"dep-inst", action == Action::"create", resource is AgentType);
"#;

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
            &TenantId::new(TENANT),
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
            "description": "identity dependency-failure test credential",
            "created_by": "test",
            "expires_at": ""
        }),
    )
    .await;
    assert!(response.success, "Issue: {:?}", response.error);
    key_hash
}

/// Build a journal-backed (Sim) platform with agent specs bootstrapped and
/// the dependency-test HTTP policy loaded. Returns the state plus a handle
/// to the backing `SimEventStore` for fault injection.
async fn dependency_test_state() -> (PlatformState, SimEventStore) {
    let mut state = PlatformState::new(None);
    bootstrap_system_tenant(&state, &BTreeMap::new());
    bootstrap_agent_specs(&state, TENANT, false, &BTreeMap::new());

    let sim_store = SimEventStore::no_faults(0);
    state
        .server
        .set_storage_stack(StorageStack::from_sim(sim_store.clone(), None));

    state
        .server
        .authz
        .reload_tenant_policies(TENANT, POLICY)
        .expect("dependency test policy should parse");

    (state, sim_store)
}

fn persistence_id(entity_type: &str, entity_id: &str) -> String {
    format!("{TENANT}:{entity_type}:{entity_id}")
}

// =========================================================================
// Positive controls
// =========================================================================

/// A healthy store resolves and authenticates a valid credential normally.
#[tokio::test]
async fn valid_identity_resolves_and_authenticates() {
    let (state, _sim_store) = dependency_test_state().await;
    let plaintext = "tmpr_dependency-valid-key";
    define_type_and_issue_credential(&state, "dep-type", "dep-agent", plaintext, "dep-inst").await;

    let app = temper_platform::router::build_platform_router(state);
    let response = app
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", format!("Bearer {plaintext}"))
                .header("X-Tenant-Id", TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a healthy store must authenticate a valid credential"
    );
}

/// A revoked credential remains denied (401), not reclassified as a
/// dependency failure.
#[tokio::test]
async fn revoked_credential_remains_401_not_503() {
    let (state, _sim_store) = dependency_test_state().await;
    let plaintext = "tmpr_dependency-revoked-key";
    let key_hash = define_type_and_issue_credential(
        &state,
        "dep-type-revoked",
        "dep-agent",
        plaintext,
        "dep-inst",
    )
    .await;
    let response = dispatch(
        &state,
        "AgentCredential",
        &key_hash,
        "Revoke",
        serde_json::json!({}),
    )
    .await;
    assert!(response.success, "Revoke: {:?}", response.error);

    let app = temper_platform::router::build_platform_router(state);
    let response = app
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", format!("Bearer {plaintext}"))
                .header("X-Tenant-Id", TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "a genuinely revoked credential must stay 401, not become 503"
    );
}

/// A credential presented against a different, equally-governed tenant
/// remains denied (401) — this is a confirmed absence (the credential was
/// never issued there), not a dependency failure. Distinct from, but with
/// the same outcome as, a tenant with no governance at all — see
/// `ungoverned_tenant_falls_through_to_401_not_503`: both are configuration
/// states, not outages.
#[tokio::test]
async fn wrong_tenant_credential_remains_401_not_503() {
    let (state, _sim_store) = dependency_test_state().await;
    bootstrap_agent_specs(&state, OTHER_GOVERNED_TENANT, false, &BTreeMap::new());
    let plaintext = "tmpr_dependency-wrong-tenant-key";
    define_type_and_issue_credential(&state, "dep-type-wt", "dep-agent", plaintext, "dep-inst")
        .await;

    let app = temper_platform::router::build_platform_router(state);
    let response = app
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", format!("Bearer {plaintext}"))
                .header("X-Tenant-Id", OTHER_GOVERNED_TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "a credential scoped to a different, equally-governed tenant must stay 401, not become 503"
    );
}

/// A never-issued token is denied (401), not reported as a dependency
/// failure — confirmed absence stays `Invalid`.
#[tokio::test]
async fn never_registered_token_remains_401_not_503() {
    let (state, _sim_store) = dependency_test_state().await;
    let app = temper_platform::router::build_platform_router(state);
    let response = app
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", "Bearer tmpr_never-registered")
                .header("X-Tenant-Id", TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "a token that was never registered is a confirmed absence, not a dependency failure"
    );
}

/// A tenant with no governing transition table for `AgentCredential` at all
/// (never bootstrapped with agent specs) is a configuration state — identity
/// governance was never installed there — not an outage. It must fall
/// through exactly like any other confirmed-absent credential: 401, not 503.
///
/// This inverts the contract this suite originally shipped with (CI caught
/// the error: `crates/temper-platform/src/bearer_auth/tests.rs` fixtures
/// that never call `bootstrap_agent_specs` — `PlatformState::new(None)` with
/// no bootstrap at all — hit exactly this path and got 503 instead of their
/// expected 200/401). Only genuine infrastructure failures (store/journal
/// read or replay errors, actor-ask failures, a poisoned registry lock)
/// remain `Unavailable` → 503; see `credential_replay_failure_is_503_not_401`,
/// `linked_agent_type_replay_failure_is_503_not_401`,
/// `both_credential_reads_faulted_is_503_not_401`, and
/// `poisoned_registry_lock_is_503_not_401` below for those.
#[tokio::test]
async fn ungoverned_tenant_falls_through_to_401_not_503() {
    let (state, _sim_store) = dependency_test_state().await;
    // "ungoverned-tenant" never ran `bootstrap_agent_specs`, so it has no
    // AgentCredential/AgentType transition table at all.
    let app = temper_platform::router::build_platform_router(state);
    let response = app
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", "Bearer tmpr_whatever")
                .header("X-Tenant-Id", "ungoverned-tenant")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "a tenant with identity governance not installed is a configuration \
         state, not a dependency outage — it must fall through to the \
         pre-existing denial path (401), never become 503"
    );
}

// =========================================================================
// Dependency failures: fail closed as 503, never 401/404/anonymous
// =========================================================================

/// A faulted `AgentCredential` journal replay must surface as 503, not 401.
#[tokio::test]
async fn credential_replay_failure_is_503_not_401() {
    let (state, sim_store) = dependency_test_state().await;
    let plaintext = "tmpr_dependency-credential-fault-key";
    let key_hash = define_type_and_issue_credential(
        &state,
        "dep-type-cred-fault",
        "dep-agent",
        plaintext,
        "dep-inst",
    )
    .await;

    sim_store.fail_next_reads(&persistence_id("AgentCredential", &key_hash), 1);

    let app = temper_platform::router::build_platform_router(state);
    let response = app
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", format!("Bearer {plaintext}"))
                .header("X-Tenant-Id", TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a replay failure on the AgentCredential read must fail closed as a \
         dependency failure (503), never as an invalid credential (401)"
    );
}

/// A faulted linked `AgentType` journal replay must surface as 503, not 401.
#[tokio::test]
async fn linked_agent_type_replay_failure_is_503_not_401() {
    let (state, sim_store) = dependency_test_state().await;
    let plaintext = "tmpr_dependency-type-fault-key";
    define_type_and_issue_credential(
        &state,
        "dep-type-type-fault",
        "dep-agent",
        plaintext,
        "dep-inst",
    )
    .await;

    sim_store.fail_next_reads(&persistence_id("AgentType", "dep-type-type-fault"), 1);

    let app = temper_platform::router::build_platform_router(state);
    let response = app
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", format!("Bearer {plaintext}"))
                .header("X-Tenant-Id", TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a replay failure on the linked AgentType read must fail closed as a \
         dependency failure (503), never as an invalid credential (401)"
    );
}

/// Both the initial `AgentCredential` read and its recheck read go through
/// the identical classification helper (see `classify_dependency_read`); a
/// fault spanning both reads of one resolution must still fail closed as
/// 503. The in-memory fixture (`SimEventStore::fail_next_reads`) has no
/// "skip N then fail" primitive, so the initial-read and recheck-read call
/// sites cannot be independently targeted within a single resolution — this
/// test demonstrates the shared outcome; the recheck call site's own code
/// (identical to the initial read's `classify_dependency_read(...)?`,
/// verified by code review) is exercised at the unit level in
/// `temper_server::identity::error::tests::classify_dependency_read_covers_every_resolver_call_site`.
#[tokio::test]
async fn both_credential_reads_faulted_is_503_not_401() {
    let (state, sim_store) = dependency_test_state().await;
    let plaintext = "tmpr_dependency-recheck-fault-key";
    let key_hash = define_type_and_issue_credential(
        &state,
        "dep-type-recheck-fault",
        "dep-agent",
        plaintext,
        "dep-inst",
    )
    .await;

    sim_store.fail_next_reads(&persistence_id("AgentCredential", &key_hash), 2);

    let app = temper_platform::router::build_platform_router(state);
    let response = app
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", format!("Bearer {plaintext}"))
                .header("X-Tenant-Id", TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a fault spanning the credential's initial read and its recheck \
         must fail closed as 503, never as 401"
    );
}

/// A poisoned spec-registry lock must surface as 503, not 401 — the
/// registry's own unavailability must never be silently treated as "no
/// such credential".
#[tokio::test]
async fn poisoned_registry_lock_is_503_not_401() {
    let (state, _sim_store) = dependency_test_state().await;
    let plaintext = "tmpr_dependency-registry-poison-key";
    define_type_and_issue_credential(
        &state,
        "dep-type-poison",
        "dep-agent",
        plaintext,
        "dep-inst",
    )
    .await;

    let registry = state.server.registry.clone();
    let _ = std::thread::spawn(move || {
        let _guard = registry.write().expect("lock not yet poisoned");
        panic!("deliberately poison the spec registry lock for this test");
    })
    .join();
    assert!(
        state.server.registry.read().is_err(),
        "the registry lock must actually be poisoned before the assertion below is meaningful"
    );

    let app = temper_platform::router::build_platform_router(state);
    let response = app
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", format!("Bearer {plaintext}"))
                .header("X-Tenant-Id", TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "a poisoned spec-registry lock must fail closed as a dependency \
         failure (503), never as an invalid credential (401)"
    );
}

/// The `/api/identity/resolve` endpoint must map a dependency failure to
/// 503 with a JSON error body, not 404 ("credential not found").
#[tokio::test]
async fn identity_resolve_endpoint_dependency_failure_is_503_not_404() {
    let (state, sim_store) = dependency_test_state().await;
    let plaintext = "tmpr_dependency-resolve-endpoint-fault-key";
    let key_hash = define_type_and_issue_credential(
        &state,
        "dep-type-resolve-fault",
        "dep-agent",
        plaintext,
        "dep-inst",
    )
    .await;
    sim_store.fail_next_reads(&persistence_id("AgentCredential", &key_hash), 1);

    let app = temper_platform::router::build_platform_router(state);
    let response = app
        .oneshot(
            Request::post("/api/identity/resolve")
                .header("Content-Type", "application/json")
                .header("X-Tenant-Id", TENANT)
                .body(Body::from(format!(r#"{{"bearer_token": "{plaintext}"}}"#)))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::SERVICE_UNAVAILABLE,
        "the identity resolve endpoint must report a dependency failure as \
         503, never as 404 (\"credential not found\")"
    );
    let json = body_json(response).await;
    assert!(
        json.get("error").is_some(),
        "the 503 body must carry a JSON error field: {json:?}"
    );
}

/// The store self-heals: once the injected fault is consumed, the exact
/// same credential resolves and authenticates again.
#[tokio::test]
async fn store_restored_after_fault_resolves_successfully() {
    let (state, sim_store) = dependency_test_state().await;
    let plaintext = "tmpr_dependency-recovers-key";
    let key_hash = define_type_and_issue_credential(
        &state,
        "dep-type-recovers",
        "dep-agent",
        plaintext,
        "dep-inst",
    )
    .await;
    sim_store.fail_next_reads(&persistence_id("AgentCredential", &key_hash), 1);

    let app = temper_platform::router::build_platform_router(state);

    let faulted = app
        .clone()
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", format!("Bearer {plaintext}"))
                .header("X-Tenant-Id", TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(faulted.status(), StatusCode::SERVICE_UNAVAILABLE);

    let recovered = app
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", format!("Bearer {plaintext}"))
                .header("X-Tenant-Id", TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        recovered.status(),
        StatusCode::OK,
        "once the store recovers, the exact same credential must resolve \
         and authenticate again"
    );
}

// =========================================================================
// No fallback, no mutation, no leaking into anonymous/protocol forwarding
// =========================================================================

/// A dependency failure during bearer resolution must never reach the
/// protected handler — a mutating request made with a faulted credential
/// must not take effect.
#[tokio::test]
async fn dependency_failure_never_reaches_the_protected_handler() {
    let (state, sim_store) = dependency_test_state().await;
    let plaintext = "tmpr_dependency-no-mutation-key";
    let key_hash = define_type_and_issue_credential(
        &state,
        "dep-type-no-mutation",
        "dep-agent",
        plaintext,
        "dep-inst",
    )
    .await;
    sim_store.fail_next_reads(&persistence_id("AgentCredential", &key_hash), 1);

    let server = state.server.clone();
    let app = temper_platform::router::build_platform_router(state);
    let response = app
        .oneshot(
            Request::post("/tdata/AgentTypes")
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {plaintext}"))
                .header("X-Tenant-Id", TENANT)
                .body(Body::from(r#"{"id": "should-never-be-created"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        !server.entity_exists(
            &TenantId::new(TENANT),
            "AgentType",
            "should-never-be-created"
        ),
        "a dependency failure during bearer resolution must never reach the \
         protected handler and must never mutate state"
    );
}

/// A dependency failure during bearer resolution must not be served as an
/// anonymous/successful request either.
#[tokio::test]
async fn dependency_failure_does_not_fall_back_to_anonymous() {
    let (state, sim_store) = dependency_test_state().await;
    let plaintext = "tmpr_dependency-no-anon-fallback-key";
    let key_hash = define_type_and_issue_credential(
        &state,
        "dep-type-no-anon",
        "dep-agent",
        plaintext,
        "dep-inst",
    )
    .await;
    sim_store.fail_next_reads(&persistence_id("AgentCredential", &key_hash), 1);

    let app = temper_platform::router::build_platform_router(state);
    let response = app
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", format!("Bearer {plaintext}"))
                .header("X-Tenant-Id", TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(
        response.status(),
        StatusCode::OK,
        "a dependency failure must never be served as an anonymous/successful request"
    );
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

// =========================================================================
// Control: authz/edge behavior for resolved principals is unchanged
// =========================================================================

/// A principal that resolves successfully but is denied by Cedar policy
/// still gets 403 — identity dependency-error handling must not alter
/// authz/edge guard behavior for principals that DO resolve.
#[tokio::test]
async fn resolved_principal_denied_by_policy_is_still_403() {
    let (state, _sim_store) = dependency_test_state().await;
    let plaintext = "tmpr_dependency-authz-denied-key";
    // Issued but with no policy grant for this instance id — the HTTP
    // policy only grants "dep-inst", not "unpermitted-inst".
    define_type_and_issue_credential(
        &state,
        "dep-type-authz-denied",
        "dep-agent",
        plaintext,
        "unpermitted-inst",
    )
    .await;

    let app = temper_platform::router::build_platform_router(state);
    let response = app
        .oneshot(
            Request::get("/tdata/AgentTypes")
                .header("Authorization", format!("Bearer {plaintext}"))
                .header("X-Tenant-Id", TENANT)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::FORBIDDEN,
        "a principal that resolves but is denied by Cedar policy must still \
         get 403 — authz/edge guard behavior is unchanged by this change"
    );
}
