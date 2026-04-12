//! End-to-end HTTP validation of the Environment constraint surface.
//!
//! Every branch of the Crucible constraint surface is exercised through the
//! real OData router — POST/PATCH/action requests flow through
//! `build_router` → `run_write_prechecks` →
//! `pre_upsert_field_invariant_checks` / cross-invariant checks, and the
//! test asserts the full 409 JSON body shape (including
//! `error.details.type == "field_invariant" | "cross_invariant"` and the
//! configured message).
//!
//! Covers the `Local` hard constraint (ADR-0042), the `Modal` environment
//! type constraints, and cross-invariant rejection on child entities.

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::registry::{
    EntityLevelSummary, EntityVerificationResult, SpecRegistry, VerificationStatus,
};
use temper_server::{ServerState, build_router};
use temper_spec::csdl::parse_csdl;
use tower::ServiceExt;

const ENVIRONMENT_IOA: &str = include_str!("../specs/environment.ioa.toml");
const ALLOWED_HOST_IOA: &str = include_str!("../specs/environment_allowed_host.ioa.toml");
const PACKAGE_IOA: &str = include_str!("../specs/environment_package.ioa.toml");
const MANAGED_AGENT_IOA: &str = include_str!("../specs/managed_agent.ioa.toml");
const SESSION_IOA: &str = include_str!("../specs/session.ioa.toml");
const SESSION_RESOURCE_IOA: &str = include_str!("../specs/session_resource.ioa.toml");
const MEMORY_STORE_IOA: &str = include_str!("../specs/memory_store.ioa.toml");
const MEMORY_IOA: &str = include_str!("../specs/memory.ioa.toml");
const MEMORY_VERSION_IOA: &str = include_str!("../specs/memory_version.ioa.toml");
const SESSION_SCHEDULE_IOA: &str = include_str!("../specs/session_schedule.ioa.toml");
const CRUCIBLE_SCHEDULER_IOA: &str = include_str!("../specs/crucible_scheduler.ioa.toml");
const CROSS_INVARIANTS_TOML: &str = include_str!("../specs/cross-invariants.toml");
const MODEL_CSDL: &str = include_str!("../specs/model.csdl.xml");

/// Build a `ServerState` preloaded with Crucible's three IOAs, CSDL, and
/// the cross-invariants file. Marks all three entity types as verified so
/// the verification gate lets writes through — these tests exercise the
/// constraint pipeline, not the cascade.
fn build_crucible_state() -> ServerState {
    let csdl = parse_csdl(MODEL_CSDL).expect("crucible CSDL should parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant_with_reactions_and_constraints(
        "default",
        csdl,
        MODEL_CSDL.to_string(),
        &[
            ("Environment", ENVIRONMENT_IOA),
            ("EnvironmentAllowedHost", ALLOWED_HOST_IOA),
            ("EnvironmentPackage", PACKAGE_IOA),
            ("ManagedAgent", MANAGED_AGENT_IOA),
            ("Session", SESSION_IOA),
            ("SessionResource", SESSION_RESOURCE_IOA),
            ("MemoryStore", MEMORY_STORE_IOA),
            ("Memory", MEMORY_IOA),
            ("MemoryVersion", MEMORY_VERSION_IOA),
            ("SessionSchedule", SESSION_SCHEDULE_IOA),
            ("CrucibleScheduler", CRUCIBLE_SCHEDULER_IOA),
        ],
        Vec::new(),
        Some(CROSS_INVARIANTS_TOML.to_string()),
    );

    let system = ActorSystem::new("crucible-validation");
    let state = ServerState::from_registry(system, registry);

    // Mark all three entity types verified so write requests aren't rejected
    // by the verification gate. The cascade test covers verification; this
    // test covers the constraint pipeline.
    {
        let mut registry = state.registry.write().unwrap();
        for entity_type in [
            "Environment",
            "EnvironmentAllowedHost",
            "EnvironmentPackage",
            "ManagedAgent",
            "Session",
            "SessionResource",
            "MemoryStore",
            "Memory",
            "MemoryVersion",
            "SessionSchedule",
            "CrucibleScheduler",
        ] {
            registry.set_verification_status(
                &TenantId::default(),
                entity_type,
                VerificationStatus::Completed(EntityVerificationResult {
                    all_passed: true,
                    levels: vec![EntityLevelSummary {
                        level: "L0 SMT".to_string(),
                        passed: true,
                        summary: "OK".to_string(),
                        details: None,
                    }],
                    verified_at: "2026-04-11T00:00:00Z".to_string(),
                }),
            );
        }
    }
    state
}

/// Send an HTTP request through the router and return `(status, body_json)`.
async fn send(
    state: &ServerState,
    method: Method,
    uri: &str,
    body: &str,
) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let json = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn post(state: &ServerState, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
    send(state, Method::POST, uri, body).await
}

async fn patch(state: &ServerState, uri: &str, body: &str) -> (StatusCode, serde_json::Value) {
    send(state, Method::PATCH, uri, body).await
}

async fn delete(state: &ServerState, uri: &str) -> (StatusCode, serde_json::Value) {
    send(state, Method::DELETE, uri, "").await
}

// =========================================================================
// LOCAL HAPPY PATH
// =========================================================================

#[tokio::test]
async fn local_unrestricted_environment_is_allowed() {
    let state = build_crucible_state();
    let (status, body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-local-ok",
            "Name": "local-dev",
            "ConfigType": "Local",
            "NetworkingType": "Unrestricted"
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "Local + Unrestricted must be allowed: {body:?}"
    );
}

// =========================================================================
// LOCAL HARD CONSTRAINTS — all three field invariants
// =========================================================================

#[tokio::test]
async fn local_limited_networking_is_rejected() {
    let state = build_crucible_state();
    let (status, body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-local-limited",
            "Name": "bad-local",
            "ConfigType": "Local",
            "NetworkingType": "Limited"
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Local + Limited networking must be rejected: {body:?}"
    );
    assert_eq!(body["error"]["code"].as_str(), Some("ConstraintViolation"));
    assert_eq!(
        body["error"]["details"]["type"].as_str(),
        Some("field_invariant")
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("LocalNetworkingMustBeUnrestricted")
    );
    assert_eq!(
        body["error"]["message"].as_str(),
        Some("Local environments must use Unrestricted networking")
    );
}

#[tokio::test]
async fn local_allow_mcp_servers_is_rejected() {
    let state = build_crucible_state();
    let (status, body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-local-mcp",
            "Name": "bad-local-mcp",
            "ConfigType": "Local",
            "NetworkingType": "Unrestricted",
            "AllowMcpServers": true
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Local + AllowMcpServers must be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["type"].as_str(),
        Some("field_invariant")
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("LocalCannotAllowMcpServers")
    );
}

#[tokio::test]
async fn local_allow_package_managers_is_rejected() {
    let state = build_crucible_state();
    let (status, body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-local-pkg",
            "Name": "bad-local-pkg",
            "ConfigType": "Local",
            "NetworkingType": "Unrestricted",
            "AllowPackageManagers": true
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Local + AllowPackageManagers must be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["type"].as_str(),
        Some("field_invariant")
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("LocalCannotAllowPackageManagers")
    );
}

// =========================================================================
// LOCAL — explicit AllowMcpServers=false/AllowPackageManagers=false allowed
// =========================================================================

#[tokio::test]
async fn local_with_explicit_false_flags_is_allowed() {
    // The `any_of` grammar on the field invariant permits either absence or
    // an explicit `false`. This test asserts that the explicit-false branch
    // is actually honored (catches regressions in the combinator logic).
    let state = build_crucible_state();
    let (status, body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-local-explicit-false",
            "Name": "explicit-false",
            "ConfigType": "Local",
            "NetworkingType": "Unrestricted",
            "AllowMcpServers": false,
            "AllowPackageManagers": false
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "Local with explicit false flags must be allowed: {body:?}"
    );
}

// =========================================================================
// CLOUD HAPPY PATH — full feature set + child entities
// =========================================================================

#[tokio::test]
async fn cloud_full_feature_environment_and_children_are_allowed() {
    let state = build_crucible_state();

    // Parent: full Cloud config with every cloud-only field set.
    let (status, body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-cloud-ok",
            "Name": "full-cloud",
            "ConfigType": "Cloud",
            "NetworkingType": "Limited",
            "AllowMcpServers": true,
            "AllowPackageManagers": true
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "Cloud env with all fields must be allowed: {body:?}"
    );

    // Child: allowed host attached to the Cloud parent.
    let (status, body) = post(
        &state,
        "/tdata/EnvironmentAllowedHosts",
        r#"{
            "id": "host-cloud-1",
            "EnvironmentId": "env-cloud-ok",
            "Host": "api.example.com"
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "Allowed host on Cloud parent must be allowed: {body:?}"
    );

    // Child: package attached to the Cloud parent.
    let (status, body) = post(
        &state,
        "/tdata/EnvironmentPackages",
        r#"{
            "id": "pkg-cloud-1",
            "EnvironmentId": "env-cloud-ok",
            "Manager": "Pip",
            "Name": "requests"
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "Package on Cloud parent must be allowed: {body:?}"
    );
}

// =========================================================================
// LOCAL — cross-invariant rejection on child entities
// =========================================================================

#[tokio::test]
async fn allowed_host_on_local_parent_is_rejected() {
    let state = build_crucible_state();

    // Seed a Local parent first.
    let (status, _body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-local-parent",
            "Name": "local-parent",
            "ConfigType": "Local",
            "NetworkingType": "Unrestricted"
        }"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Attempt to attach an allowed host — should be rejected by the
    // cross-invariant `AllowedHostRequiresNonLocalParent`.
    let (status, body) = post(
        &state,
        "/tdata/EnvironmentAllowedHosts",
        r#"{
            "id": "host-bad",
            "EnvironmentId": "env-local-parent",
            "Host": "api.example.com"
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Allowed host on Local parent must be rejected: {body:?}"
    );
    assert_eq!(body["error"]["code"].as_str(), Some("ConstraintViolation"));
    assert_eq!(
        body["error"]["details"]["type"].as_str(),
        Some("cross_invariant")
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("AllowedHostRequiresNonLocalParent")
    );
}

#[tokio::test]
async fn package_on_local_parent_is_rejected() {
    let state = build_crucible_state();

    let (status, _body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-local-parent-pkg",
            "Name": "local-parent-pkg",
            "ConfigType": "Local",
            "NetworkingType": "Unrestricted"
        }"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = post(
        &state,
        "/tdata/EnvironmentPackages",
        r#"{
            "id": "pkg-bad",
            "EnvironmentId": "env-local-parent-pkg",
            "Manager": "Pip",
            "Name": "requests"
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Package on Local parent must be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["type"].as_str(),
        Some("cross_invariant")
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("PackageRequiresNonLocalParent")
    );
}

// =========================================================================
// PATCH — flipping a Cloud environment to Local after setting cloud-only
// fields must be rejected.
// =========================================================================

#[tokio::test]
async fn patch_cloud_to_local_with_forbidden_fields_is_rejected() {
    let state = build_crucible_state();

    // Start with a Cloud environment that has AllowMcpServers set.
    let (status, _body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-patch-1",
            "Name": "patch-target",
            "ConfigType": "Cloud",
            "NetworkingType": "Limited",
            "AllowMcpServers": true
        }"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // PATCH it to ConfigType=Local while AllowMcpServers is still `true`.
    // The merged entity snapshot violates `LocalCannotAllowMcpServers`.
    let (status, body) = patch(
        &state,
        "/tdata/Environments('env-patch-1')",
        r#"{"ConfigType": "Local", "NetworkingType": "Unrestricted"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Flipping Cloud → Local while AllowMcpServers=true must be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["type"].as_str(),
        Some("field_invariant")
    );
}

// =========================================================================
// ARCHIVE — bound action over a seeded Local environment
// =========================================================================

#[tokio::test]
async fn archive_environment_action_succeeds() {
    let state = build_crucible_state();

    let (status, _body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-archive",
            "Name": "archive-me",
            "ConfigType": "Local",
            "NetworkingType": "Unrestricted"
        }"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = post(
        &state,
        "/tdata/Environments('env-archive')/Temper.Crucible.ArchiveEnvironment",
        "{}",
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "ArchiveEnvironment must succeed on an Active environment; got {status}: {body:?}"
    );
}

// =========================================================================
// MODAL HAPPY PATH
// =========================================================================

#[tokio::test]
async fn modal_unrestricted_environment_is_allowed() {
    let state = build_crucible_state();
    let (status, body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-modal-ok",
            "Name": "modal-sandbox",
            "ConfigType": "Modal",
            "NetworkingType": "Unrestricted",
            "ModalImage": "python:3.12-slim",
            "ModalCpu": 2.0,
            "ModalMemory": 4096,
            "ModalTimeout": 600,
            "ModalWorkdir": "/workspace"
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "Modal + Unrestricted with Modal fields must be allowed: {body:?}"
    );
}

#[tokio::test]
async fn modal_minimal_config_is_allowed() {
    let state = build_crucible_state();
    let (status, body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-modal-minimal",
            "Name": "modal-minimal",
            "ConfigType": "Modal",
            "NetworkingType": "Unrestricted"
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "Modal with only required fields must be allowed: {body:?}"
    );
}

// =========================================================================
// MODAL HARD CONSTRAINTS
// =========================================================================

#[tokio::test]
async fn modal_limited_networking_is_rejected() {
    let state = build_crucible_state();
    let (status, body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-modal-limited",
            "Name": "bad-modal",
            "ConfigType": "Modal",
            "NetworkingType": "Limited"
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Modal + Limited networking must be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("ModalNetworkingMustBeUnrestricted")
    );
}

#[tokio::test]
async fn modal_allow_mcp_servers_is_rejected() {
    let state = build_crucible_state();
    let (status, body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-modal-mcp",
            "Name": "bad-modal-mcp",
            "ConfigType": "Modal",
            "NetworkingType": "Unrestricted",
            "AllowMcpServers": true
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Modal + AllowMcpServers must be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("ModalCannotAllowMcpServers")
    );
}

#[tokio::test]
async fn modal_allow_package_managers_is_rejected() {
    let state = build_crucible_state();
    let (status, body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-modal-pkg",
            "Name": "bad-modal-pkg",
            "ConfigType": "Modal",
            "NetworkingType": "Unrestricted",
            "AllowPackageManagers": true
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Modal + AllowPackageManagers must be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("ModalCannotAllowPackageManagers")
    );
}

// =========================================================================
// LOCAL — cannot set Modal-specific fields
// =========================================================================

#[tokio::test]
async fn local_with_modal_image_is_rejected() {
    let state = build_crucible_state();
    let (status, body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-local-modal",
            "Name": "bad-local-modal",
            "ConfigType": "Local",
            "NetworkingType": "Unrestricted",
            "ModalImage": "python:3.12-slim"
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Local + ModalImage must be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("LocalCannotSetModalFields")
    );
}

#[tokio::test]
async fn local_with_modal_cpu_is_rejected() {
    let state = build_crucible_state();
    let (status, body) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-local-modal-cpu",
            "Name": "bad-local-cpu",
            "ConfigType": "Local",
            "NetworkingType": "Unrestricted",
            "ModalCpu": 4.0
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Local + ModalCpu must be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("LocalCannotSetModalFields")
    );
}

// =========================================================================
// DELETE — referential integrity
// =========================================================================

#[tokio::test]
async fn delete_unreferenced_environment_succeeds() {
    let state = build_crucible_state();

    let (status, _) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-del-ok",
            "Name": "deleteable",
            "ConfigType": "Local",
            "NetworkingType": "Unrestricted"
        }"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = delete(&state, "/tdata/Environments('env-del-ok')").await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "DELETE of unreferenced environment should succeed"
    );

    // Confirm it's gone.
    let (status, _) = send(
        &state,
        Method::GET,
        "/tdata/Environments('env-del-ok')",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn delete_environment_with_child_host_is_rejected() {
    let state = build_crucible_state();

    let (status, _) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-del-host",
            "Name": "has-host",
            "ConfigType": "Cloud",
            "NetworkingType": "Limited"
        }"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = post(
        &state,
        "/tdata/EnvironmentAllowedHosts",
        r#"{
            "id": "host-del-1",
            "EnvironmentId": "env-del-host",
            "Host": "api.example.com"
        }"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = delete(&state, "/tdata/Environments('env-del-host')").await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "DELETE with child AllowedHost must be rejected: {body:?}"
    );
    assert_eq!(body["error"]["code"].as_str(), Some("ConstraintViolation"));
}

#[tokio::test]
async fn delete_environment_with_child_package_is_rejected() {
    let state = build_crucible_state();

    let (status, _) = post(
        &state,
        "/tdata/Environments",
        r#"{
            "id": "env-del-pkg",
            "Name": "has-package",
            "ConfigType": "Cloud",
            "NetworkingType": "Unrestricted"
        }"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = post(
        &state,
        "/tdata/EnvironmentPackages",
        r#"{
            "id": "pkg-del-1",
            "EnvironmentId": "env-del-pkg",
            "Manager": "Pip",
            "Name": "requests"
        }"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = delete(&state, "/tdata/Environments('env-del-pkg')").await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "DELETE with child Package must be rejected: {body:?}"
    );
}

#[tokio::test]
async fn delete_nonexistent_environment_returns_404() {
    let state = build_crucible_state();
    let (status, _) = delete(&state, "/tdata/Environments('no-such-env')").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// =========================================================================
// MEMORY STORES — CRUD + field invariants + cross-invariants
// =========================================================================

#[tokio::test]
async fn memory_store_create_and_archive() {
    let state = build_crucible_state();

    let (status, body) = post(
        &state,
        "/tdata/MemoryStores",
        r#"{
            "id": "ms-01",
            "Name": "project-context",
            "Description": "Project conventions and preferences",
            "Status": "Active",
            "CreatedAt": "2026-04-12T00:00:00Z",
            "UpdatedAt": "2026-04-12T00:00:00Z"
        }"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create store: {body:?}");

    let (status, body) = post(
        &state,
        "/tdata/MemoryStores('ms-01')/Temper.Crucible.ArchiveMemoryStore",
        "{}",
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "archive store: {status}: {body:?}"
    );
}

#[tokio::test]
async fn memory_create_and_read() {
    let state = build_crucible_state();

    // Seed store
    post(
        &state,
        "/tdata/MemoryStores",
        r#"{"id":"ms-mem","Name":"test","Status":"Active","CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}"#,
    )
    .await;

    // Create memory
    let (status, body) = post(
        &state,
        "/tdata/Memories",
        r#"{
            "id": "mem-01",
            "MemoryStoreId": "ms-mem",
            "Path": "/preferences/formatting.md",
            "Content": "Always use 2-space indentation.",
            "ContentSha256": "abc123",
            "SizeBytes": 31,
            "CreatedAt": "2026-04-12T00:00:00Z",
            "UpdatedAt": "2026-04-12T00:00:00Z"
        }"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create memory: {body:?}");

    // Read back
    let (status, body) = send(
        &state,
        Method::GET,
        "/tdata/Memories('mem-01')",
        "",
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let fields = &body["fields"];
    assert_eq!(fields["Path"].as_str(), Some("/preferences/formatting.md"));
    assert_eq!(
        fields["Content"].as_str(),
        Some("Always use 2-space indentation.")
    );
}

#[tokio::test]
async fn memory_on_archived_store_is_rejected() {
    let state = build_crucible_state();

    // Create and archive store
    post(
        &state,
        "/tdata/MemoryStores",
        r#"{"id":"ms-arc","Name":"archived","Status":"Active","CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}"#,
    )
    .await;
    post(
        &state,
        "/tdata/MemoryStores('ms-arc')/Temper.Crucible.ArchiveMemoryStore",
        "{}",
    )
    .await;

    // Try to create memory on archived store
    let (status, body) = post(
        &state,
        "/tdata/Memories",
        r#"{
            "id": "mem-bad",
            "MemoryStoreId": "ms-arc",
            "Path": "/test.md",
            "Content": "should fail",
            "CreatedAt": "2026-04-12T00:00:00Z",
            "UpdatedAt": "2026-04-12T00:00:00Z"
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "memory on archived store must be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("MemoryRequiresActiveStore")
    );
}

#[tokio::test]
async fn memory_version_create_and_operation_invariant() {
    let state = build_crucible_state();

    // Seed store + memory
    post(&state, "/tdata/MemoryStores",
        r#"{"id":"ms-ver","Name":"ver-test","Status":"Active","CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}"#,
    ).await;
    post(&state, "/tdata/Memories",
        r#"{"id":"mem-ver","MemoryStoreId":"ms-ver","Path":"/a.md","Content":"hello","CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}"#,
    ).await;

    // Create version with valid operation
    let (status, body) = post(
        &state,
        "/tdata/MemoryVersions",
        r#"{
            "id": "mv-01",
            "MemoryId": "mem-ver",
            "MemoryStoreId": "ms-ver",
            "Operation": "created",
            "Path": "/a.md",
            "Content": "hello",
            "CreatedAt": "2026-04-12T00:00:00Z"
        }"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create version: {body:?}");

    // Invalid operation
    let (status, body) = post(
        &state,
        "/tdata/MemoryVersions",
        r#"{
            "id": "mv-bad",
            "MemoryId": "mem-ver",
            "MemoryStoreId": "ms-ver",
            "Operation": "bogus",
            "CreatedAt": "2026-04-12T00:00:00Z"
        }"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "bad operation must be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("OperationMustBeKnown")
    );
}

#[tokio::test]
async fn redact_version_clears_to_redacted_state() {
    let state = build_crucible_state();

    post(&state, "/tdata/MemoryStores",
        r#"{"id":"ms-red","Name":"redact-test","Status":"Active","CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}"#,
    ).await;
    post(&state, "/tdata/Memories",
        r#"{"id":"mem-red","MemoryStoreId":"ms-red","Path":"/secret.md","Content":"password123","CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}"#,
    ).await;
    post(&state, "/tdata/MemoryVersions",
        r#"{"id":"mv-red","MemoryId":"mem-red","MemoryStoreId":"ms-red","Operation":"created","Content":"password123","CreatedAt":"2026-04-12T00:00:00Z"}"#,
    ).await;

    let (status, body) = post(
        &state,
        "/tdata/MemoryVersions('mv-red')/Temper.Crucible.RedactVersion",
        "{}",
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "redact should succeed: {status}: {body:?}"
    );

    // Verify state is Redacted
    let (status, body) = send(&state, Method::GET, "/tdata/MemoryVersions('mv-red')", "").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"].as_str(), Some("Redacted"));
}

/// Helper to seed a minimal Environment + ManagedAgent + Session for
/// SessionResource tests that need a valid parent Session.
async fn seed_session(state: &ServerState, suffix: &str) -> String {
    let env_id = format!("env-sr-{suffix}");
    let agt_id = format!("agt-sr-{suffix}");
    let sess_id = format!("sess-sr-{suffix}");
    post(state, "/tdata/Environments", &format!(
        r#"{{"id":"{env_id}","Name":"sr-env","ConfigType":"Local","NetworkingType":"Unrestricted","CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}}"#
    )).await;
    post(state, "/tdata/ManagedAgents", &format!(
        r#"{{"id":"{agt_id}","Name":"sr-agt","Status":"Active","Version":1,"ModelId":"test","CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}}"#
    )).await;
    post(state, "/tdata/Sessions", &format!(
        r#"{{"id":"{sess_id}","AgentId":"{agt_id}","EnvironmentId":"{env_id}","Status":"Rescheduling","CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z","AgentVersion":1,"ActiveSeconds":0,"DurationSeconds":0,"InputTokens":0,"OutputTokens":0,"CacheReadInputTokens":0,"CacheCreation1hInputTokens":0,"CacheCreation5mInputTokens":0}}"#
    )).await;
    sess_id
}

#[tokio::test]
async fn session_resource_memory_store_kind() {
    let state = build_crucible_state();
    let sess_id = seed_session(&state, "ms1").await;

    let (status, body) = post(
        &state,
        "/tdata/SessionResources",
        &format!(r#"{{
            "id": "sr-ms-01",
            "SessionId": "{sess_id}",
            "Kind": "memory_store",
            "MemoryStoreId": "ms-fake",
            "Access": "read_write",
            "Prompt": "Check preferences before coding.",
            "CreatedAt": "2026-04-12T00:00:00Z",
            "UpdatedAt": "2026-04-12T00:00:00Z"
        }}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "memory_store resource should be created: {body:?}"
    );
}

#[tokio::test]
async fn session_resource_memory_store_bad_access_is_rejected() {
    let state = build_crucible_state();
    let sess_id = seed_session(&state, "ms2").await;

    let (status, body) = post(
        &state,
        "/tdata/SessionResources",
        &format!(r#"{{
            "id": "sr-ms-bad",
            "SessionId": "{sess_id}",
            "Kind": "memory_store",
            "MemoryStoreId": "ms-fake",
            "Access": "full_access",
            "CreatedAt": "2026-04-12T00:00:00Z",
            "UpdatedAt": "2026-04-12T00:00:00Z"
        }}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "bad Access must be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("MemoryStoreAccessMustBeKnown")
    );
}

#[tokio::test]
async fn session_resource_memory_store_forbids_file_fields() {
    let state = build_crucible_state();
    let sess_id = seed_session(&state, "ms3").await;

    let (status, body) = post(
        &state,
        "/tdata/SessionResources",
        &format!(r#"{{
            "id": "sr-ms-file",
            "SessionId": "{sess_id}",
            "Kind": "memory_store",
            "MemoryStoreId": "ms-fake",
            "FileId": "some-file",
            "CreatedAt": "2026-04-12T00:00:00Z",
            "UpdatedAt": "2026-04-12T00:00:00Z"
        }}"#),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "memory_store + FileId must be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("MemoryStoreResourceForbidsFileFields")
    );
}

#[tokio::test]
async fn delete_memory_store_with_memories_is_rejected() {
    let state = build_crucible_state();

    post(&state, "/tdata/MemoryStores",
        r#"{"id":"ms-del","Name":"del-test","Status":"Active","CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}"#,
    ).await;
    post(&state, "/tdata/Memories",
        r#"{"id":"mem-del","MemoryStoreId":"ms-del","Path":"/x.md","Content":"x","CreatedAt":"2026-04-12T00:00:00Z","UpdatedAt":"2026-04-12T00:00:00Z"}"#,
    ).await;

    let (status, body) = delete(&state, "/tdata/MemoryStores('ms-del')").await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "DELETE store with memories must be rejected: {body:?}"
    );
}

// =========================================================================
// SESSION SCHEDULES — cron scheduling
// =========================================================================

#[tokio::test]
async fn session_schedule_create_and_activate() {
    let state = build_crucible_state();
    let sess_id = seed_session(&state, "sched1").await;

    let (status, body) = post(
        &state,
        "/tdata/SessionSchedules",
        &format!(r#"{{
            "id": "sched-01",
            "SessionId": "{sess_id}",
            "CronExpression": "0 9 * * 1-5",
            "MessageTemplate": "Daily standup for {{{{now}}}}",
            "Status": "Draft",
            "CreatedAt": "2026-04-12T00:00:00Z",
            "UpdatedAt": "2026-04-12T00:00:00Z"
        }}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create schedule: {body:?}");

    let (status, body) = post(
        &state,
        "/tdata/SessionSchedules('sched-01')/Temper.Crucible.ActivateSchedule",
        "{}",
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "activate: {status}: {body:?}"
    );
}

#[tokio::test]
async fn session_schedule_status_invariant() {
    let state = build_crucible_state();
    let sess_id = seed_session(&state, "sched2").await;

    let (status, body) = post(
        &state,
        "/tdata/SessionSchedules",
        &format!(r#"{{
            "id": "sched-bad",
            "SessionId": "{sess_id}",
            "CronExpression": "* * * * *",
            "MessageTemplate": "test",
            "Status": "Invalid",
            "CreatedAt": "2026-04-12T00:00:00Z",
            "UpdatedAt": "2026-04-12T00:00:00Z"
        }}"#),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "bad status rejected: {body:?}");
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("StatusMustBeKnown")
    );
}

#[tokio::test]
async fn session_schedule_pause_and_resume() {
    let state = build_crucible_state();
    let sess_id = seed_session(&state, "sched3").await;

    post(
        &state,
        "/tdata/SessionSchedules",
        &format!(r#"{{
            "id": "sched-pr",
            "SessionId": "{sess_id}",
            "CronExpression": "*/5 * * * *",
            "MessageTemplate": "check",
            "Status": "Draft",
            "CreatedAt": "2026-04-12T00:00:00Z",
            "UpdatedAt": "2026-04-12T00:00:00Z"
        }}"#),
    )
    .await;

    // Activate
    post(
        &state,
        "/tdata/SessionSchedules('sched-pr')/Temper.Crucible.ActivateSchedule",
        "{}",
    )
    .await;

    // Pause
    let (status, _) = post(
        &state,
        "/tdata/SessionSchedules('sched-pr')/Temper.Crucible.PauseSchedule",
        "{}",
    )
    .await;
    assert!(status == StatusCode::OK || status == StatusCode::NO_CONTENT);

    // Resume
    let (status, _) = post(
        &state,
        "/tdata/SessionSchedules('sched-pr')/Temper.Crucible.ResumeSchedule",
        "{}",
    )
    .await;
    assert!(status == StatusCode::OK || status == StatusCode::NO_CONTENT);

    // Expire
    let (status, _) = post(
        &state,
        "/tdata/SessionSchedules('sched-pr')/Temper.Crucible.ExpireSchedule",
        "{}",
    )
    .await;
    assert!(status == StatusCode::OK || status == StatusCode::NO_CONTENT);

    // Verify expired is terminal — activate should fail
    let (status, _) = post(
        &state,
        "/tdata/SessionSchedules('sched-pr')/Temper.Crucible.ActivateSchedule",
        "{}",
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "expired should be terminal");
}

#[tokio::test]
async fn crucible_scheduler_create() {
    let state = build_crucible_state();

    let (status, body) = post(
        &state,
        "/tdata/CrucibleSchedulers",
        r#"{
            "id": "cs-test",
            "Status": "Idle",
            "HeartbeatIntervalSeconds": 30,
            "CreatedAt": "2026-04-12T00:00:00Z"
        }"#,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create scheduler: {body:?}");
}
