//! End-to-end HTTP validation of the hard `Local` constraint.
//!
//! Every branch of the Crucible constraint surface is exercised through the
//! real OData router — POST/PATCH/action requests flow through
//! `build_router` → `run_write_prechecks` →
//! `pre_upsert_field_invariant_checks` / cross-invariant checks, and the
//! test asserts the full 409 JSON body shape (including
//! `error.details.type == "field_invariant" | "cross_invariant"` and the
//! configured message).
//!
//! This is the primary correctness gate for ADR-0042's "Local environments
//! reject cloud-only fields" guarantee: the only test that proves the
//! production pipeline honors the spec end to end.

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
