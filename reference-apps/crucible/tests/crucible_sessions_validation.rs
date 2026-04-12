//! End-to-end HTTP validation of the Crucible Session slice (ADR-0044).
//!
//! Maps every endpoint from Anthropic's Beta Managed Agents Sessions API
//! onto the real Temper OData router and exercises every branch of the
//! constraint surface.
//!
//! Anthropic endpoint ↔ Crucible equivalent:
//!
//! | Anthropic                              | Crucible OData                                                              |
//! |----------------------------------------|-----------------------------------------------------------------------------|
//! | POST /v1/sessions                      | POST /tdata/Sessions                                                        |
//! | GET  /v1/sessions                      | GET  /tdata/Sessions                                                        |
//! | GET  /v1/sessions/{id}                 | GET  /tdata/Sessions('<id>')                                                |
//! | POST /v1/sessions/{id} (update)        | PATCH /tdata/Sessions('<id>')                                               |
//! | DELETE /v1/sessions/{id}               | DELETE /tdata/Sessions('<id>')                                              |
//! | POST /v1/sessions/{id}/archive         | POST /tdata/Sessions('<id>')/Temper.Crucible.ArchiveSession                 |
//! | GET  /v1/sessions/{id}/events          | GET  /tdata/SessionEvents?$filter=SessionId eq '<id>'&$orderby=Sequence asc |
//!
//! Sessions diverge from ManagedAgent in two major ways:
//!
//! 1. Real multi-state lifecycle: Rescheduling → Running → Idle → Terminated
//!    → Archived, with six bound actions exercised here end-to-end.
//! 2. Two cross-invariants on create — the session's parent ManagedAgent and
//!    Environment must both be non-Archived. The negatives cover both.

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
const AGENT_MCP_SERVER_IOA: &str = include_str!("../specs/agent_mcp_server.ioa.toml");
const AGENT_SKILL_IOA: &str = include_str!("../specs/agent_skill.ioa.toml");
const AGENT_TOOL_IOA: &str = include_str!("../specs/agent_tool.ioa.toml");
const AGENT_TOOL_CONFIG_IOA: &str = include_str!("../specs/agent_tool_config.ioa.toml");
const AGENT_VERSION_IOA: &str = include_str!("../specs/agent_version.ioa.toml");
const SESSION_IOA: &str = include_str!("../specs/session.ioa.toml");
const SESSION_RESOURCE_IOA: &str = include_str!("../specs/session_resource.ioa.toml");
const SESSION_EVENT_IOA: &str = include_str!("../specs/session_event.ioa.toml");
const CROSS_INVARIANTS_TOML: &str = include_str!("../specs/cross-invariants.toml");
const MODEL_CSDL: &str = include_str!("../specs/model.csdl.xml");

/// Build a `ServerState` preloaded with all twelve Crucible IOAs (three from
/// the Environment slice, six from the ManagedAgent slice, three from the
/// Session slice) and the extended CSDL and cross-invariants files. Marks
/// every entity type as verified so writes aren't rejected by the
/// verification gate.
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
            ("AgentMcpServer", AGENT_MCP_SERVER_IOA),
            ("AgentSkill", AGENT_SKILL_IOA),
            ("AgentTool", AGENT_TOOL_IOA),
            ("AgentToolConfig", AGENT_TOOL_CONFIG_IOA),
            ("AgentVersion", AGENT_VERSION_IOA),
            ("Session", SESSION_IOA),
            ("SessionResource", SESSION_RESOURCE_IOA),
            ("SessionEvent", SESSION_EVENT_IOA),
        ],
        Vec::new(),
        Some(CROSS_INVARIANTS_TOML.to_string()),
    );

    let system = ActorSystem::new("crucible-sessions-validation");
    let state = ServerState::from_registry(system, registry);

    {
        let mut registry = state.registry.write().unwrap();
        for entity_type in [
            "Environment",
            "EnvironmentAllowedHost",
            "EnvironmentPackage",
            "ManagedAgent",
            "AgentMcpServer",
            "AgentSkill",
            "AgentTool",
            "AgentToolConfig",
            "AgentVersion",
            "Session",
            "SessionResource",
            "SessionEvent",
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

async fn get(state: &ServerState, uri: &str) -> (StatusCode, serde_json::Value) {
    send(state, Method::GET, uri, "").await
}

async fn delete(state: &ServerState, uri: &str) -> (StatusCode, serde_json::Value) {
    send(state, Method::DELETE, uri, "").await
}

// --- Parent fixture helpers -------------------------------------------------

/// Create a Cloud environment (no Local restrictions; safe to use as parent).
async fn make_environment(state: &ServerState, id: &str) {
    let body = format!(
        r#"{{
            "id": "{id}",
            "Name": "env-{id}",
            "Status": "Active",
            "ConfigType": "Cloud",
            "NetworkingType": "Unrestricted",
            "AllowMcpServers": true,
            "AllowPackageManagers": true,
            "CreatedAt": "2026-04-11T00:00:00Z",
            "UpdatedAt": "2026-04-11T00:00:00Z"
        }}"#
    );
    let (status, body_out) = post(state, "/tdata/Environments", &body).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "environment fixture must be created: {body_out:?}"
    );
}

async fn make_managed_agent(state: &ServerState, id: &str) {
    let body = format!(
        r#"{{
            "id": "{id}",
            "Name": "agent-{id}",
            "ModelId": "claude-sonnet-4-6",
            "Status": "Active",
            "Version": 1,
            "CreatedAt": "2026-04-11T00:00:00Z",
            "UpdatedAt": "2026-04-11T00:00:00Z"
        }}"#
    );
    let (status, body_out) = post(state, "/tdata/ManagedAgents", &body).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "managed-agent fixture must be created: {body_out:?}"
    );
}

/// Canonical "good" Session POST body referencing pre-created parent fixtures.
fn session_body(id: &str, agent_id: &str, env_id: &str) -> String {
    format!(
        r#"{{
            "id": "{id}",
            "AgentId": "{agent_id}",
            "EnvironmentId": "{env_id}",
            "Title": "research-session-{id}",
            "Metadata": "{{\"purpose\":\"experiment\"}}",
            "Status": "Rescheduling",
            "CreatedAt": "2026-04-11T00:00:00Z",
            "UpdatedAt": "2026-04-11T00:00:00Z"
        }}"#
    )
}

async fn make_session(state: &ServerState, id: &str, agent_id: &str, env_id: &str) {
    make_environment(state, env_id).await;
    make_managed_agent(state, agent_id).await;
    let (status, body_out) = post(state, "/tdata/Sessions", &session_body(id, agent_id, env_id)).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "session fixture must be created: {body_out:?}"
    );
}

// =========================================================================
// POST /v1/sessions  →  POST /tdata/Sessions
// =========================================================================

#[tokio::test]
async fn create_session_happy_path() {
    let state = build_crucible_state();
    make_environment(&state, "env-1").await;
    make_managed_agent(&state, "agent-1").await;

    let (status, body) = post(
        &state,
        "/tdata/Sessions",
        &session_body("sess-1", "agent-1", "env-1"),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "canonical session must be accepted: {body:?}"
    );
}

#[tokio::test]
async fn create_session_with_unknown_status_is_rejected() {
    let state = build_crucible_state();
    make_environment(&state, "env-unk").await;
    make_managed_agent(&state, "agent-unk").await;

    let body = r#"{
        "id": "sess-bad-status",
        "AgentId": "agent-unk",
        "EnvironmentId": "env-unk",
        "Status": "Zombie",
        "CreatedAt": "2026-04-11T00:00:00Z",
        "UpdatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/Sessions", body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("StatusMustBeKnown")
    );
}

#[tokio::test]
async fn create_session_terminated_without_terminated_at_is_rejected() {
    let state = build_crucible_state();
    make_environment(&state, "env-term").await;
    make_managed_agent(&state, "agent-term").await;

    let body = r#"{
        "id": "sess-bad-term",
        "AgentId": "agent-term",
        "EnvironmentId": "env-term",
        "Status": "Terminated",
        "CreatedAt": "2026-04-11T00:00:00Z",
        "UpdatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/Sessions", body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("TerminatedRequiresTerminatedAt")
    );
}

#[tokio::test]
async fn create_session_archived_without_archived_at_is_rejected() {
    let state = build_crucible_state();
    make_environment(&state, "env-arch").await;
    make_managed_agent(&state, "agent-arch").await;

    let body = r#"{
        "id": "sess-bad-arch",
        "AgentId": "agent-arch",
        "EnvironmentId": "env-arch",
        "Status": "Archived",
        "CreatedAt": "2026-04-11T00:00:00Z",
        "UpdatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/Sessions", body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("ArchivedRequiresArchivedAt")
    );
}

#[tokio::test]
async fn create_session_on_archived_managed_agent_is_rejected() {
    let state = build_crucible_state();
    make_environment(&state, "env-dead").await;
    make_managed_agent(&state, "agent-dead").await;

    let (status, _) = post(
        &state,
        "/tdata/ManagedAgents('agent-dead')/Temper.Crucible.ArchiveManagedAgent",
        "{}",
    )
    .await;
    assert!(status == StatusCode::OK || status == StatusCode::NO_CONTENT);

    let (status, body_out) = post(
        &state,
        "/tdata/Sessions",
        &session_body("sess-orphan", "agent-dead", "env-dead"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["type"].as_str(),
        Some("cross_invariant")
    );
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("SessionRequiresActiveManagedAgent")
    );
}

#[tokio::test]
async fn create_session_on_archived_environment_is_rejected() {
    let state = build_crucible_state();
    make_environment(&state, "env-zombie").await;
    make_managed_agent(&state, "agent-alive").await;

    let (status, _) = post(
        &state,
        "/tdata/Environments('env-zombie')/Temper.Crucible.ArchiveEnvironment",
        "{}",
    )
    .await;
    assert!(status == StatusCode::OK || status == StatusCode::NO_CONTENT);

    let (status, body_out) = post(
        &state,
        "/tdata/Sessions",
        &session_body("sess-nowhere", "agent-alive", "env-zombie"),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("SessionRequiresActiveEnvironment")
    );
}

// =========================================================================
// GET /v1/sessions  →  GET /tdata/Sessions
// GET /v1/sessions/{id}  →  GET /tdata/Sessions('<id>')
// =========================================================================

#[tokio::test]
async fn list_sessions_returns_created_rows() {
    let state = build_crucible_state();
    make_environment(&state, "env-list").await;
    make_managed_agent(&state, "agent-list").await;

    let (status, _) = post(
        &state,
        "/tdata/Sessions",
        &session_body("sess-a", "agent-list", "env-list"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post(
        &state,
        "/tdata/Sessions",
        &session_body("sess-b", "agent-list", "env-list"),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = get(&state, "/tdata/Sessions").await;
    assert_eq!(status, StatusCode::OK, "list must succeed: {body:?}");
    let values = body["value"].as_array().expect("value array");
    assert!(values.len() >= 2, "list should contain both sessions");
}

#[tokio::test]
async fn get_session_by_id_returns_row() {
    let state = build_crucible_state();
    make_session(&state, "sess-get", "agent-get", "env-get").await;

    let (status, body) = get(&state, "/tdata/Sessions('sess-get')").await;
    assert_eq!(status, StatusCode::OK, "{body:?}");
    assert_eq!(body["entity_id"].as_str(), Some("sess-get"));
    assert!(body["@odata.actions"].is_array());
}

// =========================================================================
// POST /v1/sessions/{id} (update)  →  PATCH /tdata/Sessions('<id>')
// =========================================================================

#[tokio::test]
async fn patch_session_title_and_metadata() {
    let state = build_crucible_state();
    make_session(&state, "sess-patch", "agent-patch", "env-patch").await;

    let (status, body) = patch(
        &state,
        "/tdata/Sessions('sess-patch')",
        r#"{"Title": "renamed", "Metadata": "{\"purpose\":\"updated\"}", "UpdatedAt": "2026-04-11T00:05:00Z"}"#,
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "PATCH should succeed: {status} {body:?}"
    );
}

// =========================================================================
// DELETE /v1/sessions/{id}  →  DELETE /tdata/Sessions('<id>')
// =========================================================================

#[tokio::test]
async fn delete_session_succeeds() {
    let state = build_crucible_state();
    make_session(&state, "sess-del", "agent-del", "env-del").await;

    let (status, body) = delete(&state, "/tdata/Sessions('sess-del')").await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "DELETE should succeed: {status} {body:?}"
    );
}

// =========================================================================
// Session lifecycle bound actions
// =========================================================================

#[tokio::test]
async fn lifecycle_start_idle_resume_terminate_archive() {
    let state = build_crucible_state();
    make_session(&state, "sess-lc", "agent-lc", "env-lc").await;

    // Rescheduling → Running
    let (status, body) = post(
        &state,
        "/tdata/Sessions('sess-lc')/Temper.Crucible.StartSession",
        "{}",
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "StartSession: {status} {body:?}"
    );

    // Running → Idle
    let (status, body) = post(
        &state,
        "/tdata/Sessions('sess-lc')/Temper.Crucible.IdleSession",
        "{}",
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "IdleSession: {status} {body:?}"
    );

    // Idle → Running
    let (status, body) = post(
        &state,
        "/tdata/Sessions('sess-lc')/Temper.Crucible.ResumeSession",
        "{}",
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "ResumeSession: {status} {body:?}"
    );

    // Set TerminatedAt before the Terminated transition — field invariants
    // fire on bound actions too, so TerminatedRequiresTerminatedAt would
    // reject the transition otherwise.
    let (status, body) = patch(
        &state,
        "/tdata/Sessions('sess-lc')",
        r#"{"TerminatedAt":"2026-04-11T00:05:00Z"}"#,
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "PATCH TerminatedAt: {status} {body:?}"
    );

    // Running → Terminated
    let (status, body) = post(
        &state,
        "/tdata/Sessions('sess-lc')/Temper.Crucible.TerminateSession",
        "{}",
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "TerminateSession: {status} {body:?}"
    );

    // Set ArchivedAt before the Archived transition, same reasoning.
    let (status, body) = patch(
        &state,
        "/tdata/Sessions('sess-lc')",
        r#"{"ArchivedAt":"2026-04-11T00:10:00Z"}"#,
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "PATCH ArchivedAt: {status} {body:?}"
    );

    // Terminated → Archived
    let (status, body) = post(
        &state,
        "/tdata/Sessions('sess-lc')/Temper.Crucible.ArchiveSession",
        "{}",
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "ArchiveSession: {status} {body:?}"
    );
}

#[tokio::test]
async fn reschedule_roundtrip_from_running_and_idle() {
    let state = build_crucible_state();
    make_session(&state, "sess-rs", "agent-rs", "env-rs").await;

    // Rescheduling → Running → Rescheduling → Running → Idle → Rescheduling
    let (status, _) = post(
        &state,
        "/tdata/Sessions('sess-rs')/Temper.Crucible.StartSession",
        "{}",
    )
    .await;
    assert!(status == StatusCode::OK || status == StatusCode::NO_CONTENT);

    let (status, body) = post(
        &state,
        "/tdata/Sessions('sess-rs')/Temper.Crucible.RescheduleSession",
        "{}",
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "RescheduleSession from Running: {status} {body:?}"
    );

    // Back to Running, then Idle, then Reschedule from Idle
    let (_, _) = post(
        &state,
        "/tdata/Sessions('sess-rs')/Temper.Crucible.StartSession",
        "{}",
    )
    .await;
    let (_, _) = post(
        &state,
        "/tdata/Sessions('sess-rs')/Temper.Crucible.IdleSession",
        "{}",
    )
    .await;
    let (status, body) = post(
        &state,
        "/tdata/Sessions('sess-rs')/Temper.Crucible.RescheduleSession",
        "{}",
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "RescheduleSession from Idle: {status} {body:?}"
    );
}

#[tokio::test]
async fn archive_from_running_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ar-run", "agent-ar", "env-ar").await;

    let (_, _) = post(
        &state,
        "/tdata/Sessions('sess-ar-run')/Temper.Crucible.StartSession",
        "{}",
    )
    .await;

    // Must not be allowed to archive directly from Running — terminate first.
    let (status, body) = post(
        &state,
        "/tdata/Sessions('sess-ar-run')/Temper.Crucible.ArchiveSession",
        "{}",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "ArchiveSession from Running must be rejected: {body:?}"
    );
}

#[tokio::test]
async fn start_from_running_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ss-run", "agent-ss", "env-ss").await;

    let (_, _) = post(
        &state,
        "/tdata/Sessions('sess-ss-run')/Temper.Crucible.StartSession",
        "{}",
    )
    .await;

    // Second StartSession from Running must be rejected.
    let (status, body) = post(
        &state,
        "/tdata/Sessions('sess-ss-run')/Temper.Crucible.StartSession",
        "{}",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "StartSession from Running must be rejected: {body:?}"
    );
}

#[tokio::test]
async fn terminate_from_multiple_states_is_accepted() {
    // Verifies the multi-state `from = ["Rescheduling", "Running", "Idle"]`
    // wiring for TerminateSession. Runs three sessions, one per origin state.
    let state = build_crucible_state();

    // From Rescheduling (initial)
    make_session(&state, "sess-t-rs", "agent-t1", "env-t1").await;
    let (status, _) = post(
        &state,
        "/tdata/Sessions('sess-t-rs')/Temper.Crucible.TerminateSession",
        "{}",
    )
    .await;
    assert!(status == StatusCode::OK || status == StatusCode::NO_CONTENT);

    // From Running
    make_session(&state, "sess-t-run", "agent-t2", "env-t2").await;
    let (_, _) = post(
        &state,
        "/tdata/Sessions('sess-t-run')/Temper.Crucible.StartSession",
        "{}",
    )
    .await;
    let (status, _) = post(
        &state,
        "/tdata/Sessions('sess-t-run')/Temper.Crucible.TerminateSession",
        "{}",
    )
    .await;
    assert!(status == StatusCode::OK || status == StatusCode::NO_CONTENT);

    // From Idle
    make_session(&state, "sess-t-idle", "agent-t3", "env-t3").await;
    let (_, _) = post(
        &state,
        "/tdata/Sessions('sess-t-idle')/Temper.Crucible.StartSession",
        "{}",
    )
    .await;
    let (_, _) = post(
        &state,
        "/tdata/Sessions('sess-t-idle')/Temper.Crucible.IdleSession",
        "{}",
    )
    .await;
    let (status, _) = post(
        &state,
        "/tdata/Sessions('sess-t-idle')/Temper.Crucible.TerminateSession",
        "{}",
    )
    .await;
    assert!(status == StatusCode::OK || status == StatusCode::NO_CONTENT);
}

// =========================================================================
// SessionResource — discriminator enforcement (seven field invariants)
// =========================================================================

#[tokio::test]
async fn create_github_resource_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-res-g", "agent-res-g", "env-res-g").await;

    let body = r#"{
        "id": "res-g-1",
        "SessionId": "sess-res-g",
        "Kind": "github_repository",
        "MountPath": "/workspace/repo",
        "Url": "https://github.com/example/repo",
        "CheckoutKind": "branch",
        "CheckoutRef": "main",
        "CreatedAt": "2026-04-11T00:00:00Z",
        "UpdatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/SessionResources", body).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "github resource must be accepted: {body_out:?}"
    );
}

#[tokio::test]
async fn create_file_resource_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-res-f", "agent-res-f", "env-res-f").await;

    let body = r#"{
        "id": "res-f-1",
        "SessionId": "sess-res-f",
        "Kind": "file",
        "MountPath": "/workspace/uploads/data.csv",
        "FileId": "file_abc123",
        "CreatedAt": "2026-04-11T00:00:00Z",
        "UpdatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/SessionResources", body).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "file resource must be accepted: {body_out:?}"
    );
}

#[tokio::test]
async fn create_resource_with_unknown_kind_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-res-u", "agent-res-u", "env-res-u").await;

    let body = r#"{
        "id": "res-bad-kind",
        "SessionId": "sess-res-u",
        "Kind": "smoke-signal",
        "CreatedAt": "2026-04-11T00:00:00Z",
        "UpdatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/SessionResources", body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("KindMustBeKnown")
    );
}

#[tokio::test]
async fn github_resource_without_url_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-res-n", "agent-res-n", "env-res-n").await;

    let body = r#"{
        "id": "res-g-no-url",
        "SessionId": "sess-res-n",
        "Kind": "github_repository",
        "CreatedAt": "2026-04-11T00:00:00Z",
        "UpdatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/SessionResources", body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("GithubResourceRequiresUrl")
    );
}

#[tokio::test]
async fn github_resource_with_file_id_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-res-fi", "agent-res-fi", "env-res-fi").await;

    let body = r#"{
        "id": "res-g-fi",
        "SessionId": "sess-res-fi",
        "Kind": "github_repository",
        "Url": "https://github.com/example/repo",
        "FileId": "should-not-be-here",
        "CreatedAt": "2026-04-11T00:00:00Z",
        "UpdatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/SessionResources", body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("GithubResourceForbidsFileId")
    );
}

#[tokio::test]
async fn file_resource_with_url_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-res-fu", "agent-res-fu", "env-res-fu").await;

    let body = r#"{
        "id": "res-f-url",
        "SessionId": "sess-res-fu",
        "Kind": "file",
        "FileId": "file_123",
        "Url": "https://github.com/example/repo",
        "CreatedAt": "2026-04-11T00:00:00Z",
        "UpdatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/SessionResources", body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("FileResourceForbidsGithubFields")
    );
}

#[tokio::test]
async fn checkout_ref_without_checkout_kind_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-res-cr", "agent-res-cr", "env-res-cr").await;

    let body = r#"{
        "id": "res-g-dangling-ref",
        "SessionId": "sess-res-cr",
        "Kind": "github_repository",
        "Url": "https://github.com/example/repo",
        "CheckoutRef": "main",
        "CreatedAt": "2026-04-11T00:00:00Z",
        "UpdatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/SessionResources", body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("CheckoutRefRequiresCheckoutKind")
    );
}

#[tokio::test]
async fn create_resource_on_archived_session_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-dead", "agent-dead2", "env-dead2").await;

    // Set timestamps first so the field invariants don't block the transitions.
    let (st, body) = patch(
        &state,
        "/tdata/Sessions('sess-dead')",
        r#"{"TerminatedAt":"2026-04-11T00:05:00Z","ArchivedAt":"2026-04-11T00:10:00Z"}"#,
    )
    .await;
    assert!(
        st == StatusCode::OK || st == StatusCode::NO_CONTENT,
        "PATCH timestamps: {st} {body:?}"
    );

    // Terminate + archive the session
    let (st, body) = post(
        &state,
        "/tdata/Sessions('sess-dead')/Temper.Crucible.TerminateSession",
        "{}",
    )
    .await;
    assert!(
        st == StatusCode::OK || st == StatusCode::NO_CONTENT,
        "TerminateSession: {st} {body:?}"
    );
    let (st, body) = post(
        &state,
        "/tdata/Sessions('sess-dead')/Temper.Crucible.ArchiveSession",
        "{}",
    )
    .await;
    assert!(
        st == StatusCode::OK || st == StatusCode::NO_CONTENT,
        "ArchiveSession: {st} {body:?}"
    );

    let body = r#"{
        "id": "res-too-late",
        "SessionId": "sess-dead",
        "Kind": "file",
        "FileId": "file_late",
        "CreatedAt": "2026-04-11T00:00:00Z",
        "UpdatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/SessionResources", body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["type"].as_str(),
        Some("cross_invariant")
    );
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("ResourceRequiresNonArchivedSession")
    );
}

// =========================================================================
// SessionEvent — nine-branch discriminator
// =========================================================================

fn event_body(id: &str, session_id: &str, sequence: i64, kind: &str, extra: &str) -> String {
    format!(
        r#"{{
            "id": "{id}",
            "SessionId": "{session_id}",
            "Sequence": {sequence},
            "Kind": "{kind}",
            "Content": "{{\"blocks\":[]}}",
            "CreatedAt": "2026-04-11T00:00:00Z"{}{extra}
        }}"#,
        if extra.trim().is_empty() { "" } else { "," }
    )
}

#[tokio::test]
async fn create_user_message_event_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-um", "agent-ev-um", "env-ev-um").await;

    let body = event_body("ev-um", "sess-ev-um", 0, "user.message", "");
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_user_interrupt_event_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-ui", "agent-ev-ui", "env-ev-ui").await;

    let body = event_body("ev-ui", "sess-ev-ui", 0, "user.interrupt", "");
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_user_tool_confirmation_allow_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-tca", "agent-ev-tca", "env-ev-tca").await;

    let body = event_body(
        "ev-tca",
        "sess-ev-tca",
        0,
        "user.tool_confirmation",
        r#""ToolUseId": "tool_use_1", "ConfirmationResult": "allow""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_user_tool_confirmation_deny_requires_deny_message() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-tcd", "agent-ev-tcd", "env-ev-tcd").await;

    // Sad: deny without DenyMessage (ADR-0044 strict)
    let body = event_body(
        "ev-tcd-bad",
        "sess-ev-tcd",
        0,
        "user.tool_confirmation",
        r#""ToolUseId": "tool_use_1", "ConfirmationResult": "deny""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("DenyConfirmationRequiresDenyMessage")
    );

    // Happy: deny with DenyMessage
    let body = event_body(
        "ev-tcd-ok",
        "sess-ev-tcd",
        1,
        "user.tool_confirmation",
        r#""ToolUseId": "tool_use_1", "ConfirmationResult": "deny", "DenyMessage": "too destructive""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_user_tool_confirmation_without_tool_use_id_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-tcn", "agent-ev-tcn", "env-ev-tcn").await;

    let body = event_body(
        "ev-tcn-bad",
        "sess-ev-tcn",
        0,
        "user.tool_confirmation",
        r#""ConfirmationResult": "allow""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("UserToolConfirmationRequiresToolUseId")
    );
}

#[tokio::test]
async fn create_user_custom_tool_result_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-uctr", "agent-ev-uctr", "env-ev-uctr").await;

    let body = event_body(
        "ev-uctr",
        "sess-ev-uctr",
        0,
        "user.custom_tool_result",
        r#""CustomToolUseId": "ctu_2", "IsError": false"#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_user_custom_tool_result_without_custom_tool_use_id_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-uctrn", "agent-ev-uctrn", "env-ev-uctrn").await;

    let body = event_body(
        "ev-uctrn-bad",
        "sess-ev-uctrn",
        0,
        "user.custom_tool_result",
        r#""IsError": false"#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("UserCustomToolResultRequiresCustomToolUseId")
    );
}

#[tokio::test]
async fn create_agent_custom_tool_use_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-actu", "agent-ev-actu", "env-ev-actu").await;

    let body = event_body(
        "ev-actu",
        "sess-ev-actu",
        0,
        "agent.custom_tool_use",
        r#""ToolName": "calculator""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_agent_custom_tool_use_without_name_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-actun", "agent-ev-actun", "env-ev-actun").await;

    let body = event_body("ev-actun", "sess-ev-actun", 0, "agent.custom_tool_use", "");
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("AgentCustomToolUseRequiresName")
    );
}

#[tokio::test]
async fn create_agent_message_event_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-am", "agent-ev-am", "env-ev-am").await;

    let body = event_body("ev-am", "sess-ev-am", 0, "agent.message", "");
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_agent_thinking_event_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-at", "agent-ev-at", "env-ev-at").await;

    let body = event_body("ev-at", "sess-ev-at", 0, "agent.thinking", "");
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_agent_tool_use_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-atu", "agent-ev-atu", "env-ev-atu").await;

    let body = event_body(
        "ev-atu",
        "sess-ev-atu",
        0,
        "agent.tool_use",
        r#""ToolName": "web_search", "ToolUseId": "tool_use_atu_1", "EvaluatedPermission": "allow""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_agent_tool_use_without_name_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-atun", "agent-ev-atun", "env-ev-atun").await;

    let body = event_body("ev-atun-bad", "sess-ev-atun", 0, "agent.tool_use", "");
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("AgentToolUseRequiresName")
    );
}

#[tokio::test]
async fn create_agent_tool_result_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-atr", "agent-ev-atr", "env-ev-atr").await;

    let body = event_body(
        "ev-atr",
        "sess-ev-atr",
        0,
        "agent.tool_result",
        r#""ToolUseId": "tool_use_atu_1", "IsError": false"#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_agent_tool_result_without_tool_use_id_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-atrn", "agent-ev-atrn", "env-ev-atrn").await;

    let body = event_body(
        "ev-atrn-bad",
        "sess-ev-atrn",
        0,
        "agent.tool_result",
        r#""IsError": false"#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("AgentToolResultRequiresToolUseId")
    );
}

#[tokio::test]
async fn create_agent_mcp_tool_use_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-amtu", "agent-ev-amtu", "env-ev-amtu").await;

    let body = event_body(
        "ev-amtu",
        "sess-ev-amtu",
        0,
        "agent.mcp_tool_use",
        r#""ToolName": "search_web", "McpServerName": "toolbox", "EvaluatedPermission": "ask""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_agent_mcp_tool_use_missing_server_name_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-amtun", "agent-ev-amtun", "env-ev-amtun").await;

    let body = event_body(
        "ev-amtun-bad",
        "sess-ev-amtun",
        0,
        "agent.mcp_tool_use",
        r#""ToolName": "search_web""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("AgentMcpToolUseRequiresServerName")
    );
}

#[tokio::test]
async fn create_agent_mcp_tool_result_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-amtr", "agent-ev-amtr", "env-ev-amtr").await;

    let body = event_body(
        "ev-amtr",
        "sess-ev-amtr",
        0,
        "agent.mcp_tool_result",
        r#""McpToolUseId": "mtu_3", "IsError": false"#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_agent_mcp_tool_result_without_mcp_tool_use_id_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-amtrn", "agent-ev-amtrn", "env-ev-amtrn").await;

    let body = event_body(
        "ev-amtrn-bad",
        "sess-ev-amtrn",
        0,
        "agent.mcp_tool_result",
        r#""IsError": false"#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("AgentMcpToolResultRequiresMcpToolUseId")
    );
}

#[tokio::test]
async fn create_agent_thread_context_compacted_event_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-tcc", "agent-ev-tcc", "env-ev-tcc").await;

    let body = event_body(
        "ev-tcc",
        "sess-ev-tcc",
        0,
        "agent.thread_context_compacted",
        "",
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

// --- session.status_* events ---

#[tokio::test]
async fn create_session_status_running_event_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-ssr", "agent-ev-ssr", "env-ev-ssr").await;

    let body = event_body("ev-ssr", "sess-ev-ssr", 0, "session.status_running", "");
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_session_status_idle_end_turn_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-ssi", "agent-ev-ssi", "env-ev-ssi").await;

    let body = event_body(
        "ev-ssi",
        "sess-ev-ssi",
        0,
        "session.status_idle",
        r#""StopReason": "end_turn""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_session_status_idle_without_stop_reason_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-ssin", "agent-ev-ssin", "env-ev-ssin").await;

    let body = event_body("ev-ssin-bad", "sess-ev-ssin", 0, "session.status_idle", "");
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("StatusIdleRequiresStopReason")
    );
}

#[tokio::test]
async fn create_session_status_idle_requires_action_needs_event_ids() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-ssira", "agent-ev-ssira", "env-ev-ssira").await;

    // Sad: requires_action without StopReasonEventIds
    let body = event_body(
        "ev-ssira-bad",
        "sess-ev-ssira",
        0,
        "session.status_idle",
        r#""StopReason": "requires_action""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("RequiresActionRequiresEventIds")
    );

    // Happy: requires_action with StopReasonEventIds JSON blob
    let body = event_body(
        "ev-ssira-ok",
        "sess-ev-ssira",
        1,
        "session.status_idle",
        r#""StopReason": "requires_action", "StopReasonEventIds": "[\"ev-blocker-1\",\"ev-blocker-2\"]""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_session_status_idle_with_unknown_stop_reason_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-ssiunk", "agent-ev-ssiunk", "env-ev-ssiunk").await;

    let body = event_body(
        "ev-ssiunk-bad",
        "sess-ev-ssiunk",
        0,
        "session.status_idle",
        r#""StopReason": "heat_death_of_universe""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("StopReasonMustBeKnown")
    );
}

#[tokio::test]
async fn create_session_status_rescheduled_event_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-ssrs", "agent-ev-ssrs", "env-ev-ssrs").await;

    let body = event_body(
        "ev-ssrs",
        "sess-ev-ssrs",
        0,
        "session.status_rescheduled",
        "",
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_session_status_terminated_event_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-sst", "agent-ev-sst", "env-ev-sst").await;

    let body = event_body(
        "ev-sst",
        "sess-ev-sst",
        0,
        "session.status_terminated",
        "",
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_session_deleted_event_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-sd", "agent-ev-sd", "env-ev-sd").await;

    let body = event_body("ev-sd", "sess-ev-sd", 0, "session.deleted", "");
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

// --- session.error ---

#[tokio::test]
async fn create_session_error_event_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-se", "agent-ev-se", "env-ev-se").await;

    let body = event_body(
        "ev-se",
        "sess-ev-se",
        0,
        "session.error",
        r#""ErrorKind": "model_overloaded_error", "ErrorMessage": "upstream overloaded", "RetryStatus": "retrying""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_session_error_without_kind_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-sen", "agent-ev-sen", "env-ev-sen").await;

    let body = event_body(
        "ev-sen-bad",
        "sess-ev-sen",
        0,
        "session.error",
        r#""ErrorMessage": "something", "RetryStatus": "terminal""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("SessionErrorRequiresKind")
    );
}

#[tokio::test]
async fn create_session_error_without_message_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-senm", "agent-ev-senm", "env-ev-senm").await;

    let body = event_body(
        "ev-senm-bad",
        "sess-ev-senm",
        0,
        "session.error",
        r#""ErrorKind": "unknown_error", "RetryStatus": "terminal""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("SessionErrorRequiresMessage")
    );
}

#[tokio::test]
async fn create_session_error_without_retry_status_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-senr", "agent-ev-senr", "env-ev-senr").await;

    let body = event_body(
        "ev-senr-bad",
        "sess-ev-senr",
        0,
        "session.error",
        r#""ErrorKind": "unknown_error", "ErrorMessage": "boom""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("SessionErrorRequiresRetryStatus")
    );
}

#[tokio::test]
async fn create_session_error_with_unknown_kind_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-seku", "agent-ev-seku", "env-ev-seku").await;

    let body = event_body(
        "ev-seku-bad",
        "sess-ev-seku",
        0,
        "session.error",
        r#""ErrorKind": "volcano_error", "ErrorMessage": "hot", "RetryStatus": "terminal""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("ErrorKindMustBeKnown")
    );
}

#[tokio::test]
async fn create_session_error_with_unknown_retry_status_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-seru", "agent-ev-seru", "env-ev-seru").await;

    let body = event_body(
        "ev-seru-bad",
        "sess-ev-seru",
        0,
        "session.error",
        r#""ErrorKind": "unknown_error", "ErrorMessage": "boom", "RetryStatus": "maybe_later""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("RetryStatusMustBeKnown")
    );
}

#[tokio::test]
async fn create_mcp_error_requires_server_name() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-semcp", "agent-ev-semcp", "env-ev-semcp").await;

    // Sad: mcp_connection_failed_error without McpServerName
    let body = event_body(
        "ev-semcp-bad",
        "sess-ev-semcp",
        0,
        "session.error",
        r#""ErrorKind": "mcp_connection_failed_error", "ErrorMessage": "refused", "RetryStatus": "exhausted""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("McpErrorRequiresServerName")
    );

    // Happy: mcp_connection_failed_error with McpServerName
    let body = event_body(
        "ev-semcp-ok",
        "sess-ev-semcp",
        1,
        "session.error",
        r#""ErrorKind": "mcp_connection_failed_error", "ErrorMessage": "refused", "RetryStatus": "exhausted", "McpServerName": "toolbox""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

// --- span.model_request_* ---

#[tokio::test]
async fn create_span_model_request_start_event_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-mrs", "agent-ev-mrs", "env-ev-mrs").await;

    let body = event_body(
        "ev-mrs",
        "sess-ev-mrs",
        0,
        "span.model_request_start",
        "",
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_span_model_request_end_event_happy_path() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-mre", "agent-ev-mre", "env-ev-mre").await;

    let body = event_body(
        "ev-mre",
        "sess-ev-mre",
        0,
        "span.model_request_end",
        r#""ModelRequestStartId": "ev-mrs", "IsError": false, "ModelInputTokens": 1234, "ModelOutputTokens": 567, "ModelCacheCreationInputTokens": 0, "ModelCacheReadInputTokens": 0, "ModelSpeed": "standard""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CREATED, "{body_out:?}");
}

#[tokio::test]
async fn create_span_model_request_end_without_start_id_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-mren", "agent-ev-mren", "env-ev-mren").await;

    let body = event_body(
        "ev-mren-bad",
        "sess-ev-mren",
        0,
        "span.model_request_end",
        r#""IsError": false"#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("ModelRequestEndRequiresStartId")
    );
}

#[tokio::test]
async fn create_span_model_request_end_without_is_error_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-mreni", "agent-ev-mreni", "env-ev-mreni").await;

    let body = event_body(
        "ev-mreni-bad",
        "sess-ev-mreni",
        0,
        "span.model_request_end",
        r#""ModelRequestStartId": "ev-mrs-1""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("ModelRequestEndRequiresIsError")
    );
}

#[tokio::test]
async fn create_span_model_request_end_with_unknown_speed_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-mrens", "agent-ev-mrens", "env-ev-mrens").await;

    let body = event_body(
        "ev-mrens-bad",
        "sess-ev-mrens",
        0,
        "span.model_request_end",
        r#""ModelRequestStartId": "ev-mrs-1", "IsError": false, "ModelSpeed": "ludicrous""#,
    );
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("ModelSpeedMustBeKnown")
    );
}

#[tokio::test]
async fn create_event_with_unknown_kind_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-u", "agent-ev-u", "env-ev-u").await;

    let body = event_body("ev-bad-kind", "sess-ev-u", 0, "cosmic_ray", "");
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("KindMustBeKnown")
    );
}

#[tokio::test]
async fn create_event_on_archived_session_is_rejected() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-dead", "agent-ev-dead", "env-ev-dead").await;

    // Set timestamps first so the field invariants don't block transitions.
    let (st, body) = patch(
        &state,
        "/tdata/Sessions('sess-ev-dead')",
        r#"{"TerminatedAt":"2026-04-11T00:05:00Z","ArchivedAt":"2026-04-11T00:10:00Z"}"#,
    )
    .await;
    assert!(
        st == StatusCode::OK || st == StatusCode::NO_CONTENT,
        "PATCH timestamps: {st} {body:?}"
    );

    // Terminate + archive
    let (st, body) = post(
        &state,
        "/tdata/Sessions('sess-ev-dead')/Temper.Crucible.TerminateSession",
        "{}",
    )
    .await;
    assert!(
        st == StatusCode::OK || st == StatusCode::NO_CONTENT,
        "TerminateSession: {st} {body:?}"
    );
    let (st, body) = post(
        &state,
        "/tdata/Sessions('sess-ev-dead')/Temper.Crucible.ArchiveSession",
        "{}",
    )
    .await;
    assert!(
        st == StatusCode::OK || st == StatusCode::NO_CONTENT,
        "ArchiveSession: {st} {body:?}"
    );

    let body = event_body("ev-too-late", "sess-ev-dead", 0, "agent.message", "");
    let (status, body_out) = post(&state, "/tdata/SessionEvents", &body).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body_out:?}");
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("EventRequiresNonArchivedSession")
    );
}

#[tokio::test]
async fn list_events_for_session_by_filter() {
    let state = build_crucible_state();
    make_session(&state, "sess-ev-list", "agent-ev-list", "env-ev-list").await;

    let b0 = event_body("ev-list-0", "sess-ev-list", 0, "user.message", "");
    let (status, _) = post(&state, "/tdata/SessionEvents", &b0).await;
    assert_eq!(status, StatusCode::CREATED);
    let b1 = event_body("ev-list-1", "sess-ev-list", 1, "agent.message", "");
    let (status, _) = post(&state, "/tdata/SessionEvents", &b1).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = get(
        &state,
        "/tdata/SessionEvents?$filter=SessionId%20eq%20%27sess-ev-list%27",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "events list must succeed: {body:?}");
    let values = body["value"].as_array().expect("value array");
    assert!(
        values.len() >= 2,
        "events list should contain both created events, got {values:?}"
    );
}
