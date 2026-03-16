//! End-to-end integration test for the agent-orchestration OS app.
//!
//! Exercises the full HeartbeatRun lifecycle using the actual IOA specs,
//! CSDL, and Cedar policies from the OS app bundle with an HTTP adapter
//! backed by wiremock.

use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::registry::SpecRegistry;
use temper_server::request_context::AgentContext;
use temper_server::state::DispatchExtOptions;
use temper_spec::csdl::parse_csdl;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

// Embed the actual OS app specs (same as the bundled app).
const HEARTBEAT_IOA: &str =
    include_str!("../../../os-apps/agent-orchestration/specs/heartbeat_run.ioa.toml");
const ORG_IOA: &str =
    include_str!("../../../os-apps/agent-orchestration/specs/organization.ioa.toml");
const BUDGET_IOA: &str =
    include_str!("../../../os-apps/agent-orchestration/specs/budget_ledger.ioa.toml");
const CSDL_XML: &str = include_str!("../../../os-apps/agent-orchestration/specs/model.csdl.xml");

fn build_orchestration_state() -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("orchestration CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[
            ("HeartbeatRun", HEARTBEAT_IOA),
            ("Organization", ORG_IOA),
            ("BudgetLedger", BUDGET_IOA),
        ],
    );

    let system = ActorSystem::new("orchestration-e2e");
    ServerState::from_registry(system, registry)
}

fn supervisor_ctx() -> AgentContext {
    AgentContext {
        agent_id: Some("haku".to_string()),
        session_id: Some("test-session".to_string()),
        agent_type: Some("supervisor".to_string()),
        ..Default::default()
    }
}

fn worker_ctx(agent_id: &str) -> AgentContext {
    AgentContext {
        agent_id: Some(agent_id.to_string()),
        session_id: Some("worker-session".to_string()),
        agent_type: Some("worker".to_string()),
        ..Default::default()
    }
}

/// Helper: dispatch an action and assert success.
async fn dispatch_ok(
    state: &ServerState,
    entity_type: &str,
    entity_id: &str,
    action: &str,
    params: serde_json::Value,
    agent_ctx: &AgentContext,
    await_integration: bool,
) -> temper_server::entity_actor::EntityResponse {
    let response = state
        .dispatch_tenant_action_ext(
            &TenantId::default(),
            entity_type,
            entity_id,
            action,
            params,
            DispatchExtOptions {
                agent_ctx,
                await_integration,
            },
        )
        .await
        .unwrap_or_else(|e| panic!("{entity_type}({entity_id}).{action} failed: {e}"));
    assert!(
        response.success,
        "{entity_type}({entity_id}).{action} returned success=false: {:?}",
        response.error
    );
    response
}

// ───────────────────────────────────────────────────────────────────
// Test 1: Full HeartbeatRun lifecycle with HTTP adapter
// Schedule → ApproveBudget → CheckIn → StartExecution (adapter fires)
//   → RecordResult (via adapter callback) → Complete
// ───────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_run_full_lifecycle_with_http_adapter() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/agent/execute"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "callback_params": {
                "result": "task completed successfully"
            }
        })))
        .mount(&mock_server)
        .await;

    // Patch the HeartbeatRun spec to use HTTP adapter with wiremock URL.
    let patched_heartbeat = HEARTBEAT_IOA.replace(
        r#"adapter = "{field:adapter_type}""#,
        &format!(
            "adapter = \"http\"\nurl = \"{}/agent/execute\"\nmethod = \"POST\"\nallow_private_urls = \"true\"",
            mock_server.uri()
        ),
    );

    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[
            ("HeartbeatRun", &patched_heartbeat),
            ("Organization", ORG_IOA),
            ("BudgetLedger", BUDGET_IOA),
        ],
    );
    let system = ActorSystem::new("heartbeat-e2e");
    let state = ServerState::from_registry(system, registry);

    let sup = supervisor_ctx();
    let agent_id = "worker-claude-1";
    let worker = worker_ctx(agent_id);

    // Step 1: Schedule the run (supervisor).
    let resp = dispatch_ok(
        &state,
        "HeartbeatRun",
        "run-1",
        "Schedule",
        serde_json::json!({
            "agent_id": agent_id,
            "org_id": "org-1",
            "wake_reason": "issue-42",
            "max_turns": 10,
            "adapter_type": "http"
        }),
        &sup,
        false,
    )
    .await;
    assert_eq!(resp.state.status, "Scheduled");
    assert_eq!(
        resp.state
            .fields
            .get("agent_bound")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    // Step 2: Approve budget (supervisor).
    let resp = dispatch_ok(
        &state,
        "HeartbeatRun",
        "run-1",
        "ApproveBudget",
        serde_json::json!({}),
        &sup,
        false,
    )
    .await;
    assert_eq!(resp.state.status, "Scheduled");
    assert_eq!(
        resp.state
            .fields
            .get("budget_approved")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    // Step 3: Agent checks in (worker).
    let resp = dispatch_ok(
        &state,
        "HeartbeatRun",
        "run-1",
        "CheckIn",
        serde_json::json!({}),
        &worker,
        false,
    )
    .await;
    assert_eq!(resp.state.status, "CheckingIn");

    // Step 4: Start execution (worker) — triggers adapter integration inline.
    // The adapter calls the wiremock HTTP endpoint, which returns a success
    // callback with `result = "task completed successfully"`.
    // The dispatch pipeline fires RecordResult automatically.
    let resp = dispatch_ok(
        &state,
        "HeartbeatRun",
        "run-1",
        "StartExecution",
        serde_json::json!({}),
        &worker,
        true, // await_integration: wait for adapter + callback
    )
    .await;

    // After StartExecution + adapter success callback (RecordResult),
    // the entity should be in Working with has_result = true.
    assert_eq!(resp.state.status, "Working");
    assert_eq!(
        resp.state
            .fields
            .get("has_result")
            .and_then(|v| v.as_bool()),
        Some(true)
    );

    // Step 5: Complete the run (worker).
    let resp = dispatch_ok(
        &state,
        "HeartbeatRun",
        "run-1",
        "Complete",
        serde_json::json!({}),
        &worker,
        false,
    )
    .await;
    assert_eq!(resp.state.status, "Completed");

    // Verify the mock was called exactly once.
    let received = mock_server.received_requests().await.unwrap();
    assert_eq!(
        received.len(),
        1,
        "adapter should have made exactly one HTTP call"
    );
}

// ───────────────────────────────────────────────────────────────────
// Test 2: Adapter failure triggers Fail callback
// ───────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_run_adapter_failure_triggers_fail_callback() {
    let mock_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/agent/execute"))
        .respond_with(ResponseTemplate::new(500).set_body_string("agent crashed"))
        .mount(&mock_server)
        .await;

    let patched_heartbeat = HEARTBEAT_IOA.replace(
        r#"adapter = "{field:adapter_type}""#,
        &format!(
            "adapter = \"http\"\nurl = \"{}/agent/execute\"\nmethod = \"POST\"\nallow_private_urls = \"true\"",
            mock_server.uri()
        ),
    );

    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[
            ("HeartbeatRun", &patched_heartbeat),
            ("Organization", ORG_IOA),
            ("BudgetLedger", BUDGET_IOA),
        ],
    );
    let system = ActorSystem::new("heartbeat-failure-e2e");
    let state = ServerState::from_registry(system, registry);

    let sup = supervisor_ctx();
    let worker = worker_ctx("worker-claude-2");

    // Walk to Working state.
    dispatch_ok(
        &state,
        "HeartbeatRun",
        "run-fail",
        "Schedule",
        serde_json::json!({
            "agent_id": "worker-claude-2",
            "org_id": "org-1",
            "wake_reason": "test",
            "max_turns": 5,
            "adapter_type": "http"
        }),
        &sup,
        false,
    )
    .await;

    dispatch_ok(
        &state,
        "HeartbeatRun",
        "run-fail",
        "ApproveBudget",
        serde_json::json!({}),
        &sup,
        false,
    )
    .await;

    dispatch_ok(
        &state,
        "HeartbeatRun",
        "run-fail",
        "CheckIn",
        serde_json::json!({}),
        &worker,
        false,
    )
    .await;

    // StartExecution — adapter returns 500, on_failure triggers Fail action.
    let resp = dispatch_ok(
        &state,
        "HeartbeatRun",
        "run-fail",
        "StartExecution",
        serde_json::json!({}),
        &worker,
        true,
    )
    .await;

    // The adapter failure callback fires Fail, moving to Failed state.
    assert_eq!(resp.state.status, "Failed");
    let error_msg = resp
        .state
        .fields
        .get("error_message")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    assert!(
        !error_msg.is_empty(),
        "error_message should be set on failure"
    );
}

// ───────────────────────────────────────────────────────────────────
// Test 3: Organization lifecycle
// Setup → Configure → Activate → AddMember → RecordCost →
//   IncrementActiveRuns → DecrementActiveRuns → ResetBudgetCycle
// ───────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread")]
async fn organization_full_lifecycle() {
    let state = build_orchestration_state();
    let sup = supervisor_ctx();

    // Configure org.
    let resp = dispatch_ok(
        &state,
        "Organization",
        "org-1",
        "Configure",
        serde_json::json!({
            "name": "Temper Agents",
            "description": "Agent orchestration org",
            "budget_monthly_cents": "100000"
        }),
        &sup,
        false,
    )
    .await;
    assert_eq!(resp.state.status, "Setup");
    assert_eq!(
        resp.state.fields.get("name").and_then(|v| v.as_str()),
        Some("Temper Agents")
    );

    // Activate.
    let resp = dispatch_ok(
        &state,
        "Organization",
        "org-1",
        "Activate",
        serde_json::json!({}),
        &sup,
        false,
    )
    .await;
    assert_eq!(resp.state.status, "Active");

    // Add a member.
    let resp = dispatch_ok(
        &state,
        "Organization",
        "org-1",
        "AddMember",
        serde_json::json!({"member_id": "claude-code-1"}),
        &sup,
        false,
    )
    .await;
    assert_eq!(
        resp.state
            .fields
            .get("member_count")
            .and_then(|v| v.as_i64()),
        Some(1)
    );

    // Record cost (any agent can do this).
    let worker = worker_ctx("agent-1");
    let resp = dispatch_ok(
        &state,
        "Organization",
        "org-1",
        "RecordCost",
        serde_json::json!({
            "amount_cents": 500,
            "budget_consumed_cents": "500"
        }),
        &worker,
        false,
    )
    .await;
    assert_eq!(
        resp.state
            .fields
            .get("budget_consumed_cents")
            .and_then(|v| v.as_str()),
        Some("500")
    );

    // Increment active runs.
    let resp = dispatch_ok(
        &state,
        "Organization",
        "org-1",
        "IncrementActiveRuns",
        serde_json::json!({}),
        &worker,
        false,
    )
    .await;
    assert_eq!(
        resp.state
            .fields
            .get("active_runs")
            .and_then(|v| v.as_i64()),
        Some(1)
    );

    // Decrement active runs.
    let resp = dispatch_ok(
        &state,
        "Organization",
        "org-1",
        "DecrementActiveRuns",
        serde_json::json!({}),
        &worker,
        false,
    )
    .await;
    assert_eq!(
        resp.state
            .fields
            .get("active_runs")
            .and_then(|v| v.as_i64()),
        Some(0)
    );

    // Reset budget cycle.
    let resp = dispatch_ok(
        &state,
        "Organization",
        "org-1",
        "ResetBudgetCycle",
        serde_json::json!({"budget_consumed_cents": "0"}),
        &sup,
        false,
    )
    .await;
    assert_eq!(
        resp.state
            .fields
            .get("budget_consumed_cents")
            .and_then(|v| v.as_str()),
        Some("0")
    );
}

// ───────────────────────────────────────────────────────────────────
// Test 4: BudgetLedger append-only recording
// ───────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread")]
async fn budget_ledger_records_cost_entry() {
    let state = build_orchestration_state();
    let worker = worker_ctx("agent-1");

    let resp = dispatch_ok(
        &state,
        "BudgetLedger",
        "ledger-1",
        "Record",
        serde_json::json!({
            "org_id": "org-1",
            "agent_id": "claude-code-1",
            "run_id": "run-1",
            "amount_cents": "250",
            "tokens": "5000",
            "category": "execution"
        }),
        &worker,
        false,
    )
    .await;

    assert_eq!(resp.state.status, "Recorded");
    assert_eq!(
        resp.state.fields.get("org_id").and_then(|v| v.as_str()),
        Some("org-1")
    );
    assert_eq!(
        resp.state
            .fields
            .get("amount_cents")
            .and_then(|v| v.as_str()),
        Some("250")
    );
}

// ───────────────────────────────────────────────────────────────────
// Test 5: Guard enforcement — CheckIn requires budget_approved + agent_bound
// ───────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_run_guard_blocks_checkin_without_approval() {
    let state = build_orchestration_state();
    let sup = supervisor_ctx();
    let worker = worker_ctx("worker-1");

    // Schedule but DON'T approve budget.
    dispatch_ok(
        &state,
        "HeartbeatRun",
        "run-guard",
        "Schedule",
        serde_json::json!({
            "agent_id": "worker-1",
            "org_id": "org-1",
            "wake_reason": "test",
            "max_turns": 5,
            "adapter_type": "http"
        }),
        &sup,
        false,
    )
    .await;

    // CheckIn should fail because budget not approved.
    let result = state
        .dispatch_tenant_action_ext(
            &TenantId::default(),
            "HeartbeatRun",
            "run-guard",
            "CheckIn",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &worker,
                await_integration: false,
            },
        )
        .await;

    match result {
        Ok(resp) => {
            assert!(!resp.success, "CheckIn should fail without budget approval");
        }
        Err(_) => {
            // Also acceptable — guard violation may surface as error.
        }
    }
}

// ───────────────────────────────────────────────────────────────────
// Test 6: Cancellation from any pre-terminal state
// ───────────────────────────────────────────────────────────────────
#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_run_cancel_from_working() {
    let state = build_orchestration_state();
    let sup = supervisor_ctx();
    let worker = worker_ctx("worker-cancel");

    // Walk to Working without triggering adapter (use non-await mode).
    dispatch_ok(
        &state,
        "HeartbeatRun",
        "run-cancel",
        "Schedule",
        serde_json::json!({
            "agent_id": "worker-cancel",
            "org_id": "org-1",
            "wake_reason": "test",
            "max_turns": 5,
            "adapter_type": "http"
        }),
        &sup,
        false,
    )
    .await;
    dispatch_ok(
        &state,
        "HeartbeatRun",
        "run-cancel",
        "ApproveBudget",
        serde_json::json!({}),
        &sup,
        false,
    )
    .await;
    dispatch_ok(
        &state,
        "HeartbeatRun",
        "run-cancel",
        "CheckIn",
        serde_json::json!({}),
        &worker,
        false,
    )
    .await;
    // StartExecution without await — adapter fires in background (will fail since
    // no mock server, but we don't care because we're cancelling).
    let _ = state
        .dispatch_tenant_action_ext(
            &TenantId::default(),
            "HeartbeatRun",
            "run-cancel",
            "StartExecution",
            serde_json::json!({}),
            DispatchExtOptions {
                agent_ctx: &worker,
                await_integration: false,
            },
        )
        .await;

    // Cancel from Working.
    let resp = dispatch_ok(
        &state,
        "HeartbeatRun",
        "run-cancel",
        "Cancel",
        serde_json::json!({}),
        &worker,
        false,
    )
    .await;
    assert_eq!(resp.state.status, "Cancelled");
}
