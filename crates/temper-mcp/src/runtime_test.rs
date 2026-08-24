//! Unit tests for the MCP runtime (`runtime.rs`). Split out so `runtime.rs`
//! stays under the file-size budget; excluded from the readability ratchet as a
//! `*_test.rs` file.

use super::*;
use crate::trajectory_bounds::{MAX_TRAJECTORY_ACTIONS, MAX_TRAJECTORY_TEXT_BYTES};
use axum::{Router, extract::State, http::HeaderMap, http::StatusCode, routing::post};
use std::sync::Mutex;

#[test]
fn bounded_trajectory_actions_caps_count_and_size() {
    // ARN-222: trajectory_actions come from the full untruncated code and must
    // be bounded in both count and serialized size.
    let many: Vec<Value> = (0..MAX_TRAJECTORY_ACTIONS + 50)
        .map(|i| serde_json::json!({ "a": i }))
        .collect();
    assert_eq!(
        bounded_trajectory_actions(many).as_array().map(|a| a.len()),
        Some(MAX_TRAJECTORY_ACTIONS),
        "action count must be capped"
    );
    // One action with a huge params blob collapses to a summary.
    let huge = vec![serde_json::json!({ "params": "x".repeat(MAX_TRAJECTORY_TEXT_BYTES * 2) })];
    assert_eq!(
        bounded_trajectory_actions(huge).get("trajectory_actions_truncated"),
        Some(&serde_json::json!(true)),
        "oversized actions must collapse to a bounded summary"
    );
}

#[tokio::test]
async fn trajectory_upload_uses_identity_tenant_header() {
    // ARN-222 (wire-level): the trajectory must be filed under the authenticated
    // identity even when the code referenced other tenants — a call-site guard.
    async fn handler(
        State(captured): State<Arc<Mutex<Option<String>>>>,
        headers: HeaderMap,
    ) -> StatusCode {
        let tenant = headers
            .get("X-Tenant-Id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        *captured.lock().expect("lock") = tenant;
        StatusCode::ACCEPTED
    }

    let captured = Arc::new(Mutex::new(None));
    let app = Router::new()
        .route("/api/ots/trajectories", post(handler))
        .with_state(captured.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });

    let mut ctx = trajectory_test_ctx("my-identity");
    ctx.base_url = format!("http://{addr}");
    // Code referenced a foreign tenant far more than the identity.
    ctx.tenants_seen.insert("attacker-tenant".to_string(), 99);
    ctx.init_trajectory();
    ctx.finalize_trajectory().await;

    assert_eq!(
        captured.lock().expect("lock").as_deref(),
        Some("my-identity"),
        "trajectory must be filed under the identity tenant, not a code-referenced one"
    );
}

#[test]
fn trajectory_storage_tenant_uses_identity_not_code() {
    // ARN-222: the trajectory must be stored under the authenticated session
    // identity, never a tenant derived from the (attacker-controlled) code.
    assert_eq!(
        trajectory_storage_tenant("my-identity"),
        "my-identity",
        "code-referenced tenants must not move the trajectory off the identity"
    );
}

#[test]
fn truncate_trajectory_text_bounds_large_content() {
    // ARN-222: recorded code/result text must be bounded.
    let big = "x".repeat(MAX_TRAJECTORY_TEXT_BYTES * 4);
    let truncated = truncate_trajectory_text(&big);
    assert!(
        truncated.len() <= MAX_TRAJECTORY_TEXT_BYTES + 128,
        "recorded text must be bounded, got {} bytes",
        truncated.len()
    );
    assert!(
        truncated.contains("truncated"),
        "truncation must be marked in the recorded text"
    );
    // Text within the cap is recorded verbatim.
    assert_eq!(truncate_trajectory_text("hello"), "hello");
    // A multibyte char straddling the cap must not panic and must stay valid UTF-8.
    let multibyte = "é".repeat(MAX_TRAJECTORY_TEXT_BYTES); // 2 bytes each
    let t = truncate_trajectory_text(&multibyte);
    assert!(t.len() <= MAX_TRAJECTORY_TEXT_BYTES + 128);
    assert!(t.contains("truncated"));
}

fn trajectory_test_ctx(identity: &str) -> RuntimeContext {
    RuntimeContext {
        base_url: "http://127.0.0.1:1".to_string(),
        http: reqwest::Client::new(),
        agent_id: None,
        agent_type: None,
        session_id: None,
        api_key: None,
        identity_tenant: identity.to_string(),
        allow_host_ops: true,
        sandbox: temper_sandbox::runner::PersistentSandbox::new(&[("temper", "Temper", 1)]),
        trajectory: None,
        tenants_seen: BTreeMap::new(),
        turns_recorded: 0,
        trajectory_bytes: 0,
        capped_warned: false,
    }
}

#[test]
fn turn_cap_bounds_recorded_turns() {
    // ARN-222: the trajectory records up to MAX_TRAJECTORY_TURNS turns, then
    // drops further turns instead of growing without bound.
    let mut ctx = trajectory_test_ctx("t");
    ctx.init_trajectory();
    let ok: Result<String> = Ok("result".to_string());

    ctx.record_execute_turn("code", &ok);
    assert_eq!(ctx.turns_recorded, 1, "a turn under the cap is recorded");

    ctx.turns_recorded = MAX_TRAJECTORY_TURNS;
    ctx.record_execute_turn("code", &ok);
    assert_eq!(
        ctx.turns_recorded, MAX_TRAJECTORY_TURNS,
        "a turn at the cap is dropped, not recorded"
    );
}

#[test]
fn total_byte_budget_stops_recording() {
    // ARN-222: even within the per-turn and turn-count caps, the total recorded
    // text is bounded so the upload can't exceed the server ingest limit and be
    // silently dropped. Once the byte budget is (near) exhausted, further turns
    // are not recorded.
    use crate::trajectory_bounds::MAX_TRAJECTORY_TOTAL_BYTES;
    let mut ctx = trajectory_test_ctx("t");
    ctx.init_trajectory();
    let ok: Result<String> = Ok("r".to_string());

    // Pretend the budget is already spent.
    ctx.trajectory_bytes = MAX_TRAJECTORY_TOTAL_BYTES;
    let before = ctx.turns_recorded;
    ctx.record_execute_turn("more code", &ok);
    assert_eq!(
        ctx.turns_recorded, before,
        "a turn over the byte budget is dropped, not recorded"
    );
}

#[test]
fn worst_case_trajectory_serializes_under_server_ingest_limit() {
    // ARN-222 (the real audit-suppression guarantee): a session that records up to
    // every cap must still serialize to under the server's 2 MiB ingest limit, so
    // it is never 413'd and silently dropped. Uses control-character text (each
    // byte escapes to `\u00XX`, 6x) plus embedded actions and a failing result so
    // error_type is duplicated — every budgeted contributor at once, not just raw
    // byte length.
    const SERVER_INGEST_LIMIT: usize = 2 * 1024 * 1024;
    let mut ctx = trajectory_test_ctx("t");
    ctx.init_trajectory();

    let filler = "\u{0001}".repeat(MAX_TRAJECTORY_TEXT_BYTES);
    let code = format!("temper.action(\"t\", \"E\", \"id\", \"Act\", {{\"x\": \"{filler}\"}})");
    let err: Result<String> = Err(anyhow::anyhow!(
        "{}",
        "\u{0001}".repeat(MAX_TRAJECTORY_TEXT_BYTES)
    ));

    for _ in 0..MAX_TRAJECTORY_TURNS {
        ctx.record_execute_turn(&code, &err);
    }

    let snapshot = ctx.trajectory.as_ref().expect("trajectory").snapshot();
    let serialized = serde_json::to_string(&snapshot).expect("serialize");
    assert!(
        serialized.len() < SERVER_INGEST_LIMIT,
        "worst-case trajectory serialized to {} bytes; must stay under the {} server limit",
        serialized.len(),
        SERVER_INGEST_LIMIT
    );
}

#[test]
fn init_trajectory_resets_budgets() {
    // Fable follow-up: re-sending `initialize` must reset the turn and byte
    // budgets, not inherit a consumed one.
    let mut ctx = trajectory_test_ctx("t");
    ctx.init_trajectory();
    ctx.turns_recorded = 123;
    ctx.trajectory_bytes = 456;
    ctx.capped_warned = true;
    ctx.init_trajectory();
    assert_eq!(ctx.turns_recorded, 0);
    assert_eq!(ctx.trajectory_bytes, 0);
    assert!(
        !ctx.capped_warned,
        "the cap-warned flag must reset on re-initialize"
    );
}

#[test]
fn choice_label_does_not_panic_on_multibyte_code() {
    // ARN-222 follow-up: the choice label slices the first 100 bytes of `code`;
    // a multibyte char at the boundary must not crash the client.
    let mut ctx = trajectory_test_ctx("t");
    ctx.init_trajectory();
    let code = "é".repeat(80); // 160 bytes; byte 100 is mid-char
    let ok: Result<String> = Ok("ok".to_string());
    ctx.record_execute_turn(&code, &ok); // must not panic
    assert_eq!(ctx.turns_recorded, 1);
}
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[test]
fn extract_trajectory_actions_from_temper_action_calls() {
    let code = r#"
result = temper.action("Issue", "issue-1", "PromoteToCritical", {"Reason": "prod incident"})
other = temper.action('Issue', 'issue-1', 'Assign', {'AgentId': 'agent-2'})
tenant = temper.action("gepa-tenant", "Issues", "11111111-1111-1111-1111-111111111111", "Reassign", {"NewAssigneeId": "agent-3"})
"#;

    let actions = extract_trajectory_actions_from_code(code);
    assert_eq!(actions.len(), 3);
    assert_eq!(
        actions[0].get("action").and_then(Value::as_str),
        Some("PromoteToCritical")
    );
    assert_eq!(
        actions[2]
            .get("params")
            .and_then(Value::as_object)
            .and_then(|m| m.get("NewAssigneeId"))
            .and_then(Value::as_str),
        Some("agent-3")
    );
}

#[test]
fn normalize_python_literals_to_json() {
    let value = parse_python_json_value("{'enabled': True, 'reason': None, 'count': 2}")
        .expect("python dict should parse");
    assert_eq!(value["enabled"], serde_json::json!(true));
    assert_eq!(value["reason"], serde_json::Value::Null);
    assert_eq!(value["count"], serde_json::json!(2));
}

#[test]
fn extract_temper_call_metadata_tracks_tenant() {
    let code = r#"
await temper.action("tenant-a", "Issue", "i-1", "Assign", {"AgentId": "agent-1"})
await temper.create("tenant-b", "Task", {"Title": "x"})
"#;
    let metadata = extract_temper_call_metadata(code);
    assert!(
        metadata
            .iter()
            .any(|m| m.tenant.as_deref() == Some("tenant-a")),
        "expected tenant-a metadata"
    );
    assert!(
        metadata
            .iter()
            .any(|m| m.tenant.as_deref() == Some("tenant-b")),
        "expected tenant-b metadata"
    );
}

#[tokio::test]
async fn finalize_trajectory_retries_retryable_ots_upload_failure() {
    async fn handler(State(attempts): State<Arc<AtomicUsize>>) -> StatusCode {
        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
        if attempt == 0 {
            StatusCode::SERVICE_UNAVAILABLE
        } else {
            StatusCode::ACCEPTED
        }
    }

    let attempts = Arc::new(AtomicUsize::new(0));
    let app = Router::new()
        .route("/api/ots/trajectories", post(handler))
        .with_state(attempts.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind test server");
    let addr = listener.local_addr().expect("test server addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve test server");
    });

    let mut ctx = RuntimeContext {
        base_url: format!("http://{addr}"),
        http: reqwest::Client::new(),
        agent_id: Some("agent".to_string()),
        agent_type: Some("test-agent".to_string()),
        session_id: Some("session".to_string()),
        api_key: None,
        identity_tenant: "tenant".to_string(),
        allow_host_ops: true,
        sandbox: temper_sandbox::runner::PersistentSandbox::new(&[("temper", "Temper", 1)]),
        trajectory: None,
        tenants_seen: BTreeMap::new(),
        turns_recorded: 0,
        trajectory_bytes: 0,
        capped_warned: false,
    };
    ctx.init_trajectory();

    ctx.finalize_trajectory().await;

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}
