//! Endpoint tests for conformance checking and ATIF export.
//!
//! These drive the real router against a local Turso store seeded with kernel
//! rows and a stored OTS trajectory, so the selector, the tenant scoping, and
//! the JSON shape are all exercised rather than just the pure checker.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::{OtsTrajectoryParams, TursoEventStore, TursoTrajectoryInsert};
use tower::ServiceExt;

use crate::registry::SpecRegistry;
use crate::state::ServerState;
use crate::storage::StorageStack;

const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

async fn test_app() -> (Router, TursoEventStore) {
    build_app(AuthzMode::Permissive).await
}

/// An app whose Cedar engine carries no permits beyond the built-in
/// system-platform policy, so every non-admin principal hits default-deny.
///
/// `ServerState::from_registry` installs a permissive engine, and a gate proven
/// only against that proves nothing.
async fn strict_authz_app() -> Router {
    build_app(AuthzMode::Strict).await.0
}

enum AuthzMode {
    Permissive,
    Strict,
}

async fn build_app(authz: AuthzMode) -> (Router, TursoEventStore) {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let dir =
        std::env::temp_dir().join(format!("temper-conformance-{}-{}", std::process::id(), id));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    let db_url = format!("file:{}", dir.join("conformance.db").display());

    let turso = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");

    let csdl = parse_csdl(CSDL_XML).expect("CSDL parses");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("test-conformance"), registry);
    if matches!(authz, AuthzMode::Strict) {
        state.authz = std::sync::Arc::new(temper_authz::AuthzEngine::empty());
    }
    state.data_dir = dir;
    state.set_storage_stack(StorageStack::from_turso(turso.clone()));

    let app = Router::new()
        .nest("/api", crate::api::build_api_router())
        .with_state(state);
    (app, turso)
}

async fn seed_row(
    store: &TursoEventStore,
    action: &str,
    from: Option<&str>,
    to: Option<&str>,
    created_at: &str,
) {
    store
        .persist_trajectory(TursoTrajectoryInsert {
            tenant: "default",
            entity_type: "Order",
            entity_id: "order-1",
            action,
            success: true,
            from_status: from,
            to_status: to,
            error: None,
            agent_id: Some("agent-1"),
            session_id: Some("session-1"),
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: Some("Entity"),
            spec_governed: Some(true),
            created_at,
            request_body: None,
            intent: None,
            matched_policy_ids: None,
        })
        .await
        .expect("persist trajectory row");
}

fn admin_post(uri: &str, body: serde_json::Value, tenant: Option<&str>) -> Request<Body> {
    let mut request = Request::post(uri)
        .header("X-Temper-Principal-Kind", "admin")
        .header("Content-Type", "application/json");
    if let Some(tenant) = tenant {
        request = request.header("X-Tenant-Id", tenant);
    }
    request.body(Body::from(body.to_string())).unwrap()
}

fn admin_get(uri: &str, tenant: Option<&str>) -> Request<Body> {
    let mut request = Request::get(uri).header("X-Temper-Principal-Kind", "admin");
    if let Some(tenant) = tenant {
        request = request.header("X-Tenant-Id", tenant);
    }
    request.body(Body::empty()).unwrap()
}

async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("read body");
    serde_json::from_slice(&body).expect("body is JSON")
}

#[tokio::test]
async fn conformance_check_reports_a_clean_session() {
    let (app, store) = test_app().await;
    seed_row(
        &store,
        "AddItem",
        Some("Draft"),
        Some("Draft"),
        "2026-01-01T00:00:00Z",
    )
    .await;
    seed_row(
        &store,
        "SubmitOrder",
        Some("Draft"),
        Some("Submitted"),
        "2026-01-01T00:00:01Z",
    )
    .await;

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({"entity_type": "Order", "session_id": "session-1"}),
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    assert_eq!(body["tenant"], "default");
    assert_eq!(body["session_id"], "session-1");
    assert_eq!(body["report"]["passed"], serde_json::json!(true));
    assert_eq!(body["report"]["violations"], serde_json::json!([]));
    assert_eq!(body["report"]["stats"]["actor_rows"], serde_json::json!(2));
}

#[tokio::test]
async fn conformance_check_reports_an_illegal_transition_at_its_index() {
    let (app, store) = test_app().await;
    seed_row(
        &store,
        "AddItem",
        Some("Draft"),
        Some("Draft"),
        "2026-01-01T00:00:00Z",
    )
    .await;
    seed_row(
        &store,
        "ShipOrder",
        Some("Draft"),
        Some("Shipped"),
        "2026-01-01T00:00:01Z",
    )
    .await;

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({"entity_type": "Order", "session_id": "session-1"}),
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    assert_eq!(body["report"]["passed"], serde_json::json!(false));
    let violations = body["report"]["violations"].as_array().expect("violations");
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0]["index"], serde_json::json!(1));
    assert_eq!(violations[0]["kind"], "illegal_transition");
    assert_eq!(violations[0]["action"], "ShipOrder");
}

#[tokio::test]
async fn conformance_check_requires_an_explicit_tenant() {
    let (app, _store) = test_app().await;

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({"entity_type": "Order", "session_id": "session-1"}),
            None,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn conformance_check_rejects_an_unregistered_entity_type() {
    let (app, _store) = test_app().await;

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({"entity_type": "Nonexistent", "session_id": "session-1"}),
            Some("default"),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn conformance_check_rejects_an_out_of_range_limit() {
    let (app, _store) = test_app().await;

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({"entity_type": "Order", "session_id": "session-1", "limit": 0}),
            Some("default"),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn conformance_check_folds_in_a_stored_ots_trajectory() {
    let (app, store) = test_app().await;
    seed_row(
        &store,
        "AddItem",
        Some("Draft"),
        Some("Draft"),
        "2026-01-01T00:00:00Z",
    )
    .await;

    // The agent decided on an action the kernel never recorded a row for.
    let data = serde_json::json!({
        "trajectory_id": "traj-1",
        "version": "0.1.0",
        "metadata": {
            "task_description": "place an order",
            "timestamp_start": "2026-01-01T00:00:00Z",
            "agent_id": "agent-1",
            "outcome": "failure",
            "human_reviewed": false
        },
        "context": {},
        "turns": [{
            "turn_id": 1,
            "span_id": "span-1",
            "timestamp": "2026-01-01T00:00:00Z",
            "error": true,
            "decisions": [{
                "decision_id": "decision-1",
                "decision_type": "tool_selection",
                "choice": {"action": "Frobnicate"},
                "consequence": {"success": false}
            }]
        }]
    })
    .to_string();
    store
        .persist_ots_trajectory(&OtsTrajectoryParams {
            trajectory_id: "traj-1",
            tenant: "default",
            agent_id: "agent-1",
            session_id: "session-1",
            outcome: "failure",
            turn_count: 1,
            data: &data,
        })
        .await
        .expect("persist OTS trajectory");

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "trajectory_id": "traj-1"
            }),
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    let violations = body["report"]["violations"].as_array().expect("violations");
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0]["kind"], "unknown_action");
    assert_eq!(violations[0]["action"], "Frobnicate");
    assert_eq!(
        violations[0]["index"],
        serde_json::json!(1),
        "the OTS decision is indexed after the single kernel row"
    );
    assert_eq!(
        body["report"]["stats"]["ots_decisions_checked"],
        serde_json::json!(1)
    );
}

#[tokio::test]
async fn conformance_check_rejects_a_trajectory_from_another_session() {
    let (app, store) = test_app().await;
    seed_row(
        &store,
        "AddItem",
        Some("Draft"),
        Some("Draft"),
        "2026-01-01T00:00:00Z",
    )
    .await;
    let data = r#"{"trajectory_id":"traj-elsewhere","version":"0.1.0","metadata":{"task_description":"t","timestamp_start":"2026-01-01T00:00:00Z","agent_id":"a","outcome":"success","human_reviewed":false},"context":{}}"#;
    store
        .persist_ots_trajectory(&OtsTrajectoryParams {
            trajectory_id: "traj-elsewhere",
            tenant: "default",
            agent_id: "agent-1",
            session_id: "some-other-session",
            outcome: "success",
            turn_count: 0,
            data,
        })
        .await
        .expect("persist OTS trajectory");

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "trajectory_id": "traj-elsewhere"
            }),
            Some("default"),
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "another run's decisions must not be folded into this session's report"
    );
}

#[tokio::test]
async fn conformance_check_flags_a_truncated_session() {
    let (app, store) = test_app().await;
    seed_row(
        &store,
        "AddItem",
        Some("Draft"),
        Some("Draft"),
        "2026-01-01T00:00:00Z",
    )
    .await;
    seed_row(
        &store,
        "SubmitOrder",
        Some("Draft"),
        Some("Submitted"),
        "2026-01-01T00:00:01Z",
    )
    .await;

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({"entity_type": "Order", "session_id": "session-1", "limit": 1}),
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    assert_eq!(body["truncated"], serde_json::json!(true));
    assert_eq!(body["row_limit"], serde_json::json!(1));
    assert_eq!(body["report"]["stats"]["actor_rows"], serde_json::json!(1));
}

#[tokio::test]
async fn atif_export_returns_a_v1_7_document_with_the_stored_session() {
    let (app, store) = test_app().await;
    let data = serde_json::json!({
        "trajectory_id": "traj-atif",
        "version": "0.1.0",
        "metadata": {
            "task_description": "place an order",
            "timestamp_start": "2026-01-01T00:00:00Z",
            "agent_id": "agent-1",
            "outcome": "success",
            "human_reviewed": false,
            "harness": "temperpaw",
            "spec_version": "sha256:abcd"
        },
        "context": {},
        "turns": [{
            "turn_id": 1,
            "span_id": "span-1",
            "timestamp": "2026-01-01T00:00:00Z",
            "error": false,
            "messages": [{
                "message_id": "message-1",
                "role": "assistant",
                "timestamp": "2026-01-01T00:00:00Z",
                "content": {"type": "text", "text": "Submitting the order."}
            }],
            "decisions": [{
                "decision_id": "decision-1",
                "decision_type": "tool_selection",
                "choice": {"action": "SubmitOrder"},
                "consequence": {"success": true, "result_summary": "Submitted"},
                "cause_id": "call-1"
            }]
        }]
    })
    .to_string();
    store
        .persist_ots_trajectory(&OtsTrajectoryParams {
            trajectory_id: "traj-atif",
            tenant: "default",
            agent_id: "agent-1",
            session_id: "session-7",
            outcome: "success",
            turn_count: 1,
            data: &data,
        })
        .await
        .expect("persist OTS trajectory");

    let response = app
        .oneshot(admin_get(
            "/api/ots/trajectories/traj-atif/atif",
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    assert_eq!(body["schema_version"], "ATIF-v1.7");
    assert_eq!(body["trajectory_id"], "traj-atif");
    assert_eq!(
        body["session_id"], "session-7",
        "the session comes from the storage row, not the document"
    );
    assert_eq!(body["agent"]["name"], "temperpaw");
    assert_eq!(body["agent"]["version"], "sha256:abcd");
    let steps = body["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0]["source"], "agent");
    assert_eq!(steps[0]["message"], "Submitting the order.");
    assert_eq!(steps[0]["tool_calls"][0]["tool_call_id"], "call-1");
    assert_eq!(steps[0]["tool_calls"][0]["function_name"], "SubmitOrder");
    assert_eq!(
        steps[0]["observation"]["results"][0]["source_call_id"],
        "call-1"
    );
}

#[tokio::test]
async fn atif_export_is_scoped_to_the_requesting_tenant() {
    let (app, store) = test_app().await;
    let data = r#"{"trajectory_id":"traj-other","version":"0.1.0","metadata":{"task_description":"t","timestamp_start":"2026-01-01T00:00:00Z","agent_id":"a","outcome":"success","human_reviewed":false},"context":{}}"#;
    store
        .persist_ots_trajectory(&OtsTrajectoryParams {
            trajectory_id: "traj-other",
            tenant: "other-tenant",
            agent_id: "agent-1",
            session_id: "session-1",
            outcome: "success",
            turn_count: 0,
            data,
        })
        .await
        .expect("persist OTS trajectory");

    let response = app
        .oneshot(admin_get(
            "/api/ots/trajectories/traj-other/atif",
            Some("default"),
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::NOT_FOUND,
        "a trajectory belonging to another tenant must not be exportable"
    );
}

#[tokio::test]
async fn atif_export_reports_a_missing_trajectory_as_not_found() {
    let (app, _store) = test_app().await;

    let response = app
        .oneshot(admin_get(
            "/api/ots/trajectories/does-not-exist/atif",
            Some("default"),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn atif_export_requires_an_explicit_tenant() {
    let (app, _store) = test_app().await;

    let response = app
        .oneshot(admin_get("/api/ots/trajectories/traj-atif/atif", None))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn both_endpoints_reject_an_unauthorized_principal() {
    let app = strict_authz_app().await;

    let check = app
        .clone()
        .oneshot(
            Request::post("/api/conformance/check")
                .header("X-Temper-Principal-Kind", "agent")
                .header("X-Tenant-Id", "default")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::json!({"entity_type": "Order", "session_id": "session-1"})
                        .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(check.status(), StatusCode::FORBIDDEN);

    let export = app
        .clone()
        .oneshot(
            Request::get("/api/ots/trajectories/traj-atif/atif")
                .header("X-Temper-Principal-Kind", "agent")
                .header("X-Tenant-Id", "default")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(export.status(), StatusCode::FORBIDDEN);

    // Positive control: the same routes under the same strict engine still
    // answer an authorized caller, so the 403s above come from the gate rather
    // than from a route that never resolves.
    let admin_check = app
        .clone()
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({"entity_type": "Order", "session_id": "session-1"}),
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(admin_check.status(), StatusCode::OK);

    let admin_export = app
        .oneshot(admin_get(
            "/api/ots/trajectories/traj-atif/atif",
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(
        admin_export.status(),
        StatusCode::NOT_FOUND,
        "authorized, and the trajectory simply does not exist in this app"
    );
}
