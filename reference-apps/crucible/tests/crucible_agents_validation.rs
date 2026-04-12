//! End-to-end HTTP validation of the Crucible ManagedAgent slice (ADR-0043).
//!
//! Maps every endpoint from Anthropic's Beta Managed Agents API onto the
//! real Temper OData router and exercises every branch of the constraint
//! surface. One scenario per operation × branch (happy + forbidden).
//!
//! Anthropic endpoint ↔ Crucible equivalent:
//!
//! | Anthropic                                | Crucible OData                                                                  |
//! |------------------------------------------|---------------------------------------------------------------------------------|
//! | POST /v1/agents                          | POST /tdata/ManagedAgents                                                       |
//! | GET  /v1/agents                          | GET  /tdata/ManagedAgents                                                       |
//! | GET  /v1/agents/{id}                     | GET  /tdata/ManagedAgents('<id>')?$expand=...                                   |
//! | POST /v1/agents/{id}                     | PATCH /tdata/ManagedAgents('<id>')                                              |
//! | POST /v1/agents/{id}/archive             | POST /tdata/ManagedAgents('<id>')/Temper.Crucible.ArchiveManagedAgent           |
//! | GET  /v1/agents/{id}/versions            | GET  /tdata/AgentVersions?$filter=AgentId eq '<id>'&$orderby=Version desc       |
//!
//! Why `ManagedAgent` not `Agent`: the Temper platform's built-in Agent OS
//! app owns the `Agent` entity type and auto-installs it into every app
//! tenant on boot, which would silently hot-swap Crucible's spec at startup.
//! See `reference-apps/crucible/specs/managed_agent.ioa.toml` for the full
//! rationale.

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
const CALLABLE_AGENT_IOA: &str = include_str!("../specs/callable_agent.ioa.toml");
const CROSS_INVARIANTS_TOML: &str = include_str!("../specs/cross-invariants.toml");
const MODEL_CSDL: &str = include_str!("../specs/model.csdl.xml");

/// Build a `ServerState` preloaded with all nine Crucible IOAs (three from
/// the Environment slice + six from the ManagedAgent slice) and the extended
/// CSDL and cross-invariants files. Marks every entity type as verified so
/// writes aren't rejected by the verification gate.
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
            ("CallableAgent", CALLABLE_AGENT_IOA),
        ],
        Vec::new(),
        Some(CROSS_INVARIANTS_TOML.to_string()),
    );

    let system = ActorSystem::new("crucible-agents-validation");
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
            "CallableAgent",
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

/// Canonical "good" ManagedAgent POST body. `ModelSpeed`, `Description`, `System`,
/// and `Metadata` are optional — the default includes all of them so tests
/// that want to drop a field can clone-and-replace rather than rebuild.
fn agent_body(id: &str, name: &str) -> String {
    format!(
        r#"{{
            "id": "{id}",
            "Name": "{name}",
            "Description": "an agent",
            "System": "you are helpful",
            "ModelId": "claude-sonnet-4-6",
            "ModelSpeed": "standard",
            "Metadata": "{{\"team\":\"research\"}}",
            "Status": "Active",
            "Version": 1,
            "CreatedAt": "2026-04-11T00:00:00Z",
            "UpdatedAt": "2026-04-11T00:00:00Z"
        }}"#
    )
}

// =========================================================================
// POST /v1/agents  →  POST /tdata/ManagedAgents
// =========================================================================

#[tokio::test]
async fn create_agent_happy_path() {
    let state = build_crucible_state();
    let (status, body) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-1", "research")).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "canonical agent must be accepted: {body:?}"
    );
}

#[tokio::test]
async fn create_agent_without_optional_model_speed_is_allowed() {
    let state = build_crucible_state();
    let body = r#"{
        "id": "agent-no-speed",
        "Name": "fast-research",
        "ModelId": "claude-sonnet-4-6",
        "Status": "Active",
        "Version": 1,
        "CreatedAt": "2026-04-11T00:00:00Z",
        "UpdatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/ManagedAgents", body).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "omitting ModelSpeed must be allowed: {body_out:?}"
    );
}

#[tokio::test]
async fn create_agent_with_unknown_model_speed_is_rejected() {
    let state = build_crucible_state();
    let body = r#"{
        "id": "agent-bad-speed",
        "Name": "bad-speed",
        "ModelId": "claude-sonnet-4-6",
        "ModelSpeed": "warp",
        "Status": "Active",
        "Version": 1,
        "CreatedAt": "2026-04-11T00:00:00Z",
        "UpdatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/ManagedAgents", body).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "unknown ModelSpeed must be rejected: {body_out:?}"
    );
    assert_eq!(body_out["error"]["code"].as_str(), Some("ConstraintViolation"));
    assert_eq!(
        body_out["error"]["details"]["type"].as_str(),
        Some("field_invariant")
    );
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("ModelSpeedMustBeKnown")
    );
}

#[tokio::test]
async fn create_agent_archived_without_archived_at_is_rejected() {
    let state = build_crucible_state();
    let body = r#"{
        "id": "agent-bad-archive",
        "Name": "bad-archive",
        "ModelId": "claude-sonnet-4-6",
        "Status": "Archived",
        "Version": 1,
        "CreatedAt": "2026-04-11T00:00:00Z",
        "UpdatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/ManagedAgents", body).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "Archived without ArchivedAt must be rejected: {body_out:?}"
    );
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("ArchivedRequiresArchivedAt")
    );
}

// =========================================================================
// GET /v1/agents  →  GET /tdata/ManagedAgents
// =========================================================================

#[tokio::test]
async fn list_agents_returns_created_rows() {
    let state = build_crucible_state();
    let (status, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-a", "a")).await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-b", "b")).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = get(&state, "/tdata/ManagedAgents").await;
    assert_eq!(status, StatusCode::OK, "list must succeed: {body:?}");
    let values = body["value"].as_array().expect("value array");
    assert!(
        values.len() >= 2,
        "list should contain both created agents, got {values:?}"
    );
}

// =========================================================================
// GET /v1/agents/{id}  →  GET /tdata/ManagedAgents('<id>')
// =========================================================================

#[tokio::test]
async fn get_agent_by_id_returns_row() {
    let state = build_crucible_state();
    let (_, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-get", "get-me")).await;

    let (status, body) = get(&state, "/tdata/ManagedAgents('agent-get')").await;
    assert_eq!(status, StatusCode::OK, "get by id must succeed: {body:?}");
    // The OData entity-by-id handler returns a shape keyed by `entity_id`
    // plus `@odata.actions` / `@odata.children` enrichments — the same
    // shape existing temper-server tests rely on.
    assert_eq!(body["entity_id"].as_str(), Some("agent-get"));
    assert!(body["@odata.actions"].is_array());
}

// =========================================================================
// POST /v1/agents/{id} (update)  →  PATCH /tdata/ManagedAgents('<id>')
// =========================================================================

#[tokio::test]
async fn patch_agent_bumps_version_client_managed() {
    let state = build_crucible_state();
    let (_, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-patch", "patch-me")).await;

    // Single-writer PATCH: client computes the next Version and sends it
    // back; the server does not enforce optimistic concurrency (ADR-0043
    // Sub-Decision 1). This test asserts the PATCH is accepted and the
    // row still loads cleanly afterwards.
    let (status, body) = patch(
        &state,
        "/tdata/ManagedAgents('agent-patch')",
        r#"{"Description": "updated description", "Version": 2, "UpdatedAt": "2026-04-11T00:01:00Z"}"#,
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "PATCH should succeed: {status} {body:?}"
    );

    let (status, body) = get(&state, "/tdata/ManagedAgents('agent-patch')").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["entity_id"].as_str(), Some("agent-patch"));
}

#[tokio::test]
async fn patch_agent_to_bad_model_speed_is_rejected() {
    let state = build_crucible_state();
    let (_, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-patch-bad", "x")).await;

    let (status, body) = patch(
        &state,
        "/tdata/ManagedAgents('agent-patch-bad')",
        r#"{"ModelSpeed": "warp"}"#,
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "PATCH with unknown ModelSpeed must be rejected: {body:?}"
    );
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("ModelSpeedMustBeKnown")
    );
}

// =========================================================================
// POST /v1/agents/{id}/archive  →  POST /tdata/ManagedAgents('<id>')/Temper.Crucible.ArchiveManagedAgent
// =========================================================================

#[tokio::test]
async fn archive_agent_action_succeeds() {
    let state = build_crucible_state();
    let (_, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-arch", "archive-me")).await;

    let (status, body) = post(
        &state,
        "/tdata/ManagedAgents('agent-arch')/Temper.Crucible.ArchiveManagedAgent",
        "{}",
    )
    .await;
    assert!(
        status == StatusCode::OK || status == StatusCode::NO_CONTENT,
        "ArchiveManagedAgent must succeed on an Active managed agent; got {status}: {body:?}"
    );
}

// =========================================================================
// GET /v1/agents/{id}/versions  →  GET /tdata/AgentVersions?$filter=...
//
// AgentVersion is client-managed: the test POSTs two snapshot rows and then
// queries them back to match Anthropic's listing semantics.
// =========================================================================

#[tokio::test]
async fn list_agent_versions_after_client_snapshots() {
    let state = build_crucible_state();
    let (_, _) = post(
        &state,
        "/tdata/ManagedAgents",
        &agent_body("agent-versioned", "versioned"),
    )
    .await;

    let snapshot1 = r#"{
        "id": "ver-1",
        "AgentId": "agent-versioned",
        "Version": 1,
        "Snapshot": "{\"Name\":\"versioned\",\"Version\":1}",
        "CreatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body) = post(&state, "/tdata/AgentVersions", snapshot1).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "client snapshot POST must succeed: {body:?}"
    );

    let snapshot2 = r#"{
        "id": "ver-2",
        "AgentId": "agent-versioned",
        "Version": 2,
        "Snapshot": "{\"Name\":\"versioned\",\"Version\":2}",
        "CreatedAt": "2026-04-11T00:01:00Z"
    }"#;
    let (status, _) = post(&state, "/tdata/AgentVersions", snapshot2).await;
    assert_eq!(status, StatusCode::CREATED);

    // $filter must be URL-encoded (space → %20, apostrophe → %27) because
    // axum's URI parser rejects raw spaces.
    let (status, body) = get(
        &state,
        "/tdata/AgentVersions?$filter=AgentId%20eq%20%27agent-versioned%27",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "versions list must succeed: {body:?}");
    let values = body["value"].as_array().expect("value array");
    assert_eq!(values.len(), 2, "both snapshots must be returned");
}

// =========================================================================
// AgentMcpServer — ServerTypeMustBeUrl + McpServerRequiresActiveManagedAgent
// =========================================================================

#[tokio::test]
async fn create_mcp_server_happy_path() {
    let state = build_crucible_state();
    let (_, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-mcp", "m")).await;

    let body = r#"{
        "id": "mcp-1",
        "AgentId": "agent-mcp",
        "Name": "toolbox",
        "Type": "url",
        "Url": "https://mcp.example.com/sse"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentMcpServers", body).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "canonical MCP server must be accepted: {body_out:?}"
    );
}

#[tokio::test]
async fn create_mcp_server_with_bad_type_is_rejected() {
    let state = build_crucible_state();
    let (_, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-mcp2", "m")).await;

    let body = r#"{
        "id": "mcp-bad",
        "AgentId": "agent-mcp2",
        "Name": "toolbox",
        "Type": "stdio",
        "Url": "https://mcp.example.com/sse"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentMcpServers", body).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "non-url MCP type must be rejected: {body_out:?}"
    );
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("ServerTypeMustBeUrl")
    );
}

#[tokio::test]
async fn create_mcp_server_on_archived_agent_is_rejected() {
    let state = build_crucible_state();
    let (_, _) = post(
        &state,
        "/tdata/ManagedAgents",
        &agent_body("agent-archived-mcp", "x"),
    )
    .await;

    let (status, _) = post(
        &state,
        "/tdata/ManagedAgents('agent-archived-mcp')/Temper.Crucible.ArchiveManagedAgent",
        "{}",
    )
    .await;
    assert!(status == StatusCode::OK || status == StatusCode::NO_CONTENT);

    let body = r#"{
        "id": "mcp-after-archive",
        "AgentId": "agent-archived-mcp",
        "Name": "late",
        "Type": "url",
        "Url": "https://mcp.example.com/sse"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentMcpServers", body).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "MCP server on Archived agent must be rejected: {body_out:?}"
    );
    assert_eq!(
        body_out["error"]["details"]["type"].as_str(),
        Some("cross_invariant")
    );
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("McpServerRequiresActiveManagedAgent")
    );
}

// =========================================================================
// AgentSkill — SkillTypeMustBeKnown + SkillRequiresActiveManagedAgent
// =========================================================================

#[tokio::test]
async fn create_skill_happy_path() {
    let state = build_crucible_state();
    let (_, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-skill", "s")).await;

    let body = r#"{
        "id": "skill-1",
        "AgentId": "agent-skill",
        "SkillId": "code_review",
        "SkillType": "anthropic",
        "SkillVersion": "1.0"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentSkills", body).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "canonical skill must be accepted: {body_out:?}"
    );
}

#[tokio::test]
async fn create_skill_with_bad_type_is_rejected() {
    let state = build_crucible_state();
    let (_, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-skill2", "s")).await;

    let body = r#"{
        "id": "skill-bad",
        "AgentId": "agent-skill2",
        "SkillId": "shady",
        "SkillType": "third-party"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentSkills", body).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "unknown SkillType must be rejected: {body_out:?}"
    );
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("SkillTypeMustBeKnown")
    );
}

// =========================================================================
// AgentTool — discriminator enforcement (five field invariants)
// =========================================================================

#[tokio::test]
async fn create_agent_toolset_happy_path() {
    let state = build_crucible_state();
    let (_, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-tool-at", "t")).await;

    let body = r#"{
        "id": "tool-at-1",
        "AgentId": "agent-tool-at",
        "Kind": "agent_toolset"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentTools", body).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "agent_toolset must be accepted with no kind-specific fields: {body_out:?}"
    );
}

#[tokio::test]
async fn create_agent_toolset_with_name_is_rejected() {
    let state = build_crucible_state();
    let (_, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-tool-at2", "t")).await;

    let body = r#"{
        "id": "tool-at-bad",
        "AgentId": "agent-tool-at2",
        "Kind": "agent_toolset",
        "Name": "wrong"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentTools", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("AgentToolsetForbidsKindSpecificFields")
    );
}

#[tokio::test]
async fn create_mcp_toolset_requires_mcp_server_name() {
    let state = build_crucible_state();
    let (_, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-tool-mcp", "t")).await;

    // Happy: McpServerName present.
    let body = r#"{
        "id": "tool-mcp-ok",
        "AgentId": "agent-tool-mcp",
        "Kind": "mcp_toolset",
        "McpServerName": "toolbox"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentTools", body).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "mcp_toolset with McpServerName must be accepted: {body_out:?}"
    );

    // Sad: McpServerName missing.
    let body = r#"{
        "id": "tool-mcp-bad",
        "AgentId": "agent-tool-mcp",
        "Kind": "mcp_toolset"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentTools", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("McpToolsetRequiresMcpServerName")
    );
}

#[tokio::test]
async fn create_custom_tool_requires_name_description_input_schema() {
    let state = build_crucible_state();
    let (_, _) = post(
        &state,
        "/tdata/ManagedAgents",
        &agent_body("agent-tool-custom", "t"),
    )
    .await;

    // Happy: all three fields present.
    let body = r#"{
        "id": "tool-custom-ok",
        "AgentId": "agent-tool-custom",
        "Kind": "custom",
        "Name": "calculator",
        "Description": "arithmetic",
        "InputSchema": "{\"type\":\"object\",\"properties\":{\"a\":{\"type\":\"number\"}}}"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentTools", body).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "custom tool with all fields must be accepted: {body_out:?}"
    );

    // Sad: Name missing.
    let body = r#"{
        "id": "tool-custom-no-name",
        "AgentId": "agent-tool-custom",
        "Kind": "custom",
        "Description": "arithmetic",
        "InputSchema": "{\"type\":\"object\"}"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentTools", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("CustomToolRequiresName")
    );

    // Sad: Description missing.
    let body = r#"{
        "id": "tool-custom-no-desc",
        "AgentId": "agent-tool-custom",
        "Kind": "custom",
        "Name": "calculator",
        "InputSchema": "{\"type\":\"object\"}"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentTools", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("CustomToolRequiresDescription")
    );

    // Sad: InputSchema missing.
    let body = r#"{
        "id": "tool-custom-no-schema",
        "AgentId": "agent-tool-custom",
        "Kind": "custom",
        "Name": "calculator",
        "Description": "arithmetic"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentTools", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("CustomToolRequiresInputSchema")
    );
}

#[tokio::test]
async fn create_tool_with_unknown_kind_is_rejected() {
    let state = build_crucible_state();
    let (_, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-tool-bad", "t")).await;

    let body = r#"{
        "id": "tool-bad-kind",
        "AgentId": "agent-tool-bad",
        "Kind": "rogue"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentTools", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("KindMustBeKnown")
    );
}

// =========================================================================
// AgentToolConfig — PermissionPolicyMustBeKnown
// =========================================================================

#[tokio::test]
async fn create_tool_config_happy_path() {
    let state = build_crucible_state();
    let (_, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-cfg", "t")).await;

    let tool = r#"{
        "id": "tool-cfg-parent",
        "AgentId": "agent-cfg",
        "Kind": "agent_toolset"
    }"#;
    let (status, _) = post(&state, "/tdata/AgentTools", tool).await;
    assert_eq!(status, StatusCode::CREATED);

    let cfg = r#"{
        "id": "cfg-1",
        "AgentToolId": "tool-cfg-parent",
        "ConfigName": "web_search",
        "Enabled": true,
        "PermissionPolicy": "always_ask"
    }"#;
    let (status, body) = post(&state, "/tdata/AgentToolConfigs", cfg).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "canonical tool config must be accepted: {body:?}"
    );
}

#[tokio::test]
async fn create_tool_config_with_bad_permission_policy_is_rejected() {
    let state = build_crucible_state();
    let (_, _) = post(&state, "/tdata/ManagedAgents", &agent_body("agent-cfg2", "t")).await;

    let tool = r#"{
        "id": "tool-cfg-parent2",
        "AgentId": "agent-cfg2",
        "Kind": "agent_toolset"
    }"#;
    let (_, _) = post(&state, "/tdata/AgentTools", tool).await;

    let cfg = r#"{
        "id": "cfg-bad",
        "AgentToolId": "tool-cfg-parent2",
        "ConfigName": "web_search",
        "Enabled": true,
        "PermissionPolicy": "maybe"
    }"#;
    let (status, body) = post(&state, "/tdata/AgentToolConfigs", cfg).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body["error"]["details"]["invariant"].as_str(),
        Some("PermissionPolicyMustBeKnown")
    );
}

// =========================================================================
// AgentVersion — VersionRequiresActiveManagedAgent
// =========================================================================

#[tokio::test]
async fn create_version_on_archived_agent_is_rejected() {
    let state = build_crucible_state();
    let (_, _) = post(
        &state,
        "/tdata/ManagedAgents",
        &agent_body("agent-ver-bad", "v"),
    )
    .await;

    let (status, _) = post(
        &state,
        "/tdata/ManagedAgents('agent-ver-bad')/Temper.Crucible.ArchiveManagedAgent",
        "{}",
    )
    .await;
    assert!(status == StatusCode::OK || status == StatusCode::NO_CONTENT);

    let body = r#"{
        "id": "ver-archived",
        "AgentId": "agent-ver-bad",
        "Version": 1,
        "Snapshot": "{}",
        "CreatedAt": "2026-04-11T00:00:00Z"
    }"#;
    let (status, body_out) = post(&state, "/tdata/AgentVersions", body).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert_eq!(
        body_out["error"]["details"]["invariant"].as_str(),
        Some("VersionRequiresActiveManagedAgent")
    );
}
