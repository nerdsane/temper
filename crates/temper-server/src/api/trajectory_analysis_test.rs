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
/// system-platform policy, so every principal hits default-deny.
///
/// `ServerState::from_registry` installs a permissive engine, and a gate proven
/// only against that proves nothing.
async fn strict_authz_app() -> Router {
    build_app(AuthzMode::Strict).await.0
}

/// An app whose Cedar engine permits exactly the trajectory read these
/// endpoints gate on, and nothing else.
async fn policy_permitted_app() -> (Router, TursoEventStore) {
    build_app(AuthzMode::PermitTrajectoryReads).await
}

enum AuthzMode {
    Permissive,
    Strict,
    PermitTrajectoryReads,
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
    match authz {
        AuthzMode::Permissive => {}
        AuthzMode::Strict => {
            state.authz = std::sync::Arc::new(temper_authz::AuthzEngine::empty());
        }
        AuthzMode::PermitTrajectoryReads => {
            let engine = temper_authz::AuthzEngine::empty();
            engine
                .reload_tenant_policies(
                    "default",
                    r#"permit(principal, action == Action::"read_trajectories", resource is Trajectory);"#,
                )
                .expect("policy parses");
            state.authz = std::sync::Arc::new(engine);
        }
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
            capture_seq: None,
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

async fn response_text(response: axum::response::Response) -> String {
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("read body");
    String::from_utf8(body.to_vec()).expect("body is text")
}

/// The content hash of the spec [`build_app`] registers.
///
/// A check that names no version cannot come back `passed` — the governing
/// spec is unresolved — so every test asserting a pass names this one.
fn registered_spec_version() -> String {
    temper_store_turso::spec_content_hash(ORDER_IOA)
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
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "spec_version": registered_spec_version(),
            }),
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    assert_eq!(body["tenant"], "default");
    assert_eq!(body["session_id"], "session-1");
    assert_eq!(body["report"]["passed"], serde_json::json!(true));
    assert_eq!(body["report"]["spec_resolution"], "pinned");
    assert_eq!(
        body["report"]["evidence_complete"],
        serde_json::json!(true),
        "a resolved spec and a complete read leave nothing unseen"
    );
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
            "spec_version": "sha256:abcd",
            "agent_version": "temperpaw 3.2"
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
    assert_eq!(
        body["agent"]["version"], "temperpaw 3.2",
        "agent.version is the agent system's release"
    );
    assert_eq!(
        body["agent"]["extra"]["temper.spec_version"], "sha256:abcd",
        "the governing spec stays readable without posing as the agent release"
    );
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
}

#[tokio::test]
async fn a_self_declared_admin_header_does_not_open_either_endpoint() {
    // The principal kind is a request header the platform does not
    // authenticate, so it must buy nothing on a surface that returns one
    // named run's prompts, decisions, and request bodies.
    let app = strict_authz_app().await;

    let check = app
        .clone()
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({"entity_type": "Order", "session_id": "session-1"}),
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(
        check.status(),
        StatusCode::FORBIDDEN,
        "declaring yourself admin must not substitute for a Cedar permit"
    );

    let export = app
        .oneshot(admin_get(
            "/api/ots/trajectories/traj-atif/atif",
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(export.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn a_cedar_permitted_caller_is_let_through() {
    // Positive control for the two 403 tests above: under an engine whose only
    // permit is this read, an ordinary principal reaches both handlers, so the
    // denials come from the gate rather than from a route that never resolves.
    let (app, store) = policy_permitted_app().await;
    seed_row(
        &store,
        "AddItem",
        Some("Draft"),
        Some("Draft"),
        "2026-01-01T00:00:00Z",
    )
    .await;

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
    assert_eq!(check.status(), StatusCode::OK);

    let export = app
        .oneshot(
            Request::get("/api/ots/trajectories/traj-atif/atif")
                .header("X-Temper-Principal-Kind", "agent")
                .header("X-Tenant-Id", "default")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        export.status(),
        StatusCode::NOT_FOUND,
        "authorized, and the trajectory simply does not exist in this app"
    );
}

/// The OTS document a producer uploads: the id is top-level, as the model
/// defines it.
fn ots_upload(trajectory_id: &str) -> serde_json::Value {
    serde_json::json!({
        "trajectory_id": trajectory_id,
        "version": "0.1.0",
        "metadata": {
            "task_description": "place an order",
            "timestamp_start": "2026-01-01T00:00:00Z",
            "agent_id": "agent-1",
            "outcome": "success",
            "human_reviewed": false
        },
        "context": {},
        "turns": [{
            "turn_id": 1,
            "span_id": "span-1",
            "timestamp": "2026-01-01T00:00:00Z",
            "error": false,
            "decisions": [{
                "decision_id": "decision-1",
                "decision_type": "tool_selection",
                "choice": {"action": "SubmitOrder"},
                "consequence": {"success": true}
            }]
        }]
    })
}

#[tokio::test]
async fn a_trajectory_alone_cannot_make_a_session_pass() {
    // The end-to-end shape of the fail-open: an uploaded trajectory whose one
    // decision names a declared action (`ots_upload` sends `SubmitOrder`),
    // against a session the kernel recorded nothing for, with the spec version
    // pinned so the resolution is not the thing holding it back. Everything a
    // caller controls is present and nothing the kernel controls is.
    let (app, store) = test_app().await;
    let registered = registered_spec_version();
    let mut data = ots_upload("traj-no-rows");
    data["metadata"]["spec_version"] = serde_json::json!(format!("sha256:{registered}"));
    store
        .persist_ots_trajectory(&OtsTrajectoryParams {
            trajectory_id: "traj-no-rows",
            tenant: "default",
            agent_id: "agent-1",
            session_id: "session-1",
            outcome: "success",
            turn_count: 1,
            data: &data.to_string(),
        })
        .await
        .expect("persist OTS trajectory");

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "trajectory_id": "traj-no-rows",
            }),
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    assert_eq!(body["report"]["spec_resolution"], "pinned");
    assert_eq!(body["report"]["violations"], serde_json::json!([]));
    assert_eq!(body["report"]["stats"]["actor_rows"], serde_json::json!(0));
    assert_eq!(
        body["report"]["stats"]["ots_decisions_checked"],
        serde_json::json!(1),
        "the decision was looked at, which is what used to be mistaken for evidence"
    );
    assert_eq!(body["report"]["verdict"], "indeterminate");
    assert_eq!(
        body["report"]["passed"],
        serde_json::json!(false),
        "an uploaded trajectory is the agent's own account and cannot pass a run on its own"
    );
    assert_eq!(
        body["report"]["evidence_complete"],
        serde_json::json!(false)
    );
}

#[tokio::test]
async fn a_trajectory_stored_without_a_session_is_refused() {
    // An upload that carried no `X-Session-Id` is tied to no run. Read as a
    // wildcard it folded into whichever session the caller named, which is how
    // one unattributed upload could be presented as evidence about every run.
    let (app, store) = test_app().await;
    seed_row(
        &store,
        "AddItem",
        Some("Draft"),
        Some("Draft"),
        "2026-01-01T00:00:00Z",
    )
    .await;
    store
        .persist_ots_trajectory(&OtsTrajectoryParams {
            trajectory_id: "traj-sessionless",
            tenant: "default",
            agent_id: "agent-1",
            session_id: "",
            outcome: "success",
            turn_count: 1,
            data: &ots_upload("traj-sessionless").to_string(),
        })
        .await
        .expect("persist OTS trajectory");

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "trajectory_id": "traj-sessionless",
            }),
            Some("default"),
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "a trajectory tied to no run cannot be folded into this one"
    );
    let detail = response_text(response).await;
    assert!(
        detail.contains("without a session"),
        "the refusal must say why: {detail}"
    );
}

#[tokio::test]
async fn an_uploaded_trajectory_is_addressable_by_the_id_it_was_uploaded_with() {
    // The upload path and the read path must agree on the run's identity.
    // Seeding storage directly would prove nothing about the POST handler.
    let (app, _store) = test_app().await;

    let upload = app
        .clone()
        .oneshot(
            Request::post("/api/ots/trajectories")
                .header("X-Temper-Principal-Kind", "admin")
                .header("X-Tenant-Id", "default")
                .header("X-Session-Id", "session-42")
                .header("Content-Type", "application/json")
                .body(Body::from(ots_upload("traj-roundtrip").to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(upload.status(), StatusCode::ACCEPTED);

    let export = app
        .oneshot(admin_get(
            "/api/ots/trajectories/traj-roundtrip/atif",
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(
        export.status(),
        StatusCode::OK,
        "the uploader holds `traj-roundtrip` and must be able to read it back"
    );
    let body = json_body(export).await;
    assert_eq!(body["trajectory_id"], "traj-roundtrip");
    assert_eq!(body["session_id"], "session-42");
}

#[tokio::test]
async fn an_upload_with_misaligned_token_signals_is_rejected() {
    let (app, _store) = test_app().await;
    let mut upload = ots_upload("traj-misaligned");
    upload["turns"][0]["completion_token_ids"] = serde_json::json!([4, 5]);
    upload["turns"][0]["response_mask"] = serde_json::json!([7]);
    upload["turns"][0]["logprobs"] = serde_json::json!([-0.1, -0.2, -0.3]);

    let response = app
        .oneshot(
            Request::post("/api/ots/trajectories")
                .header("X-Temper-Principal-Kind", "admin")
                .header("X-Tenant-Id", "default")
                .header("Content-Type", "application/json")
                .body(Body::from(upload.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "positionally inconsistent RL data must not be persisted"
    );
}

#[tokio::test]
async fn a_session_ending_exactly_on_the_limit_is_not_reported_truncated() {
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
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "limit": 2,
                "spec_version": registered_spec_version(),
            }),
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    assert_eq!(
        body["truncated"],
        serde_json::json!(false),
        "a complete session that happens to be limit-sized is complete"
    );
    assert_eq!(body["report"]["verdict"], "pass");
    assert_eq!(body["report"]["passed"], serde_json::json!(true));
}

#[tokio::test]
async fn a_truncated_session_is_indeterminate_rather_than_passing() {
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
    assert_eq!(body["report"]["verdict"], "indeterminate");
    assert_eq!(
        body["report"]["passed"],
        serde_json::json!(false),
        "a consumer gating on `passed` must not accept a partially read run"
    );
}

#[tokio::test]
async fn a_session_with_no_rows_is_indeterminate_rather_than_passing() {
    let (app, _store) = test_app().await;

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({"entity_type": "Order", "session_id": "no-such-session"}),
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    assert_eq!(body["report"]["verdict"], "indeterminate");
    assert_eq!(body["report"]["passed"], serde_json::json!(false));
}

#[tokio::test]
async fn a_run_under_another_spec_version_is_refused_rather_than_judged() {
    let (app, store) = test_app().await;
    seed_row(
        &store,
        "AddItem",
        Some("Draft"),
        Some("Draft"),
        "2026-01-01T00:00:00Z",
    )
    .await;

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "spec_version": "sha256:a-spec-that-is-not-loaded"
            }),
            Some("default"),
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "judging a run by a spec it never ran under is worse than refusing"
    );
}

#[tokio::test]
async fn a_run_under_the_registered_spec_version_is_checked() {
    let (app, store) = test_app().await;
    seed_row(
        &store,
        "AddItem",
        Some("Draft"),
        Some("Draft"),
        "2026-01-01T00:00:00Z",
    )
    .await;
    let registered = temper_store_turso::spec_content_hash(ORDER_IOA);

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "spec_version": format!("sha256:{registered}")
            }),
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    assert_eq!(
        body["spec_version"], registered,
        "the report names the spec it was produced against"
    );
}

#[tokio::test]
async fn an_ots_trajectory_declaring_another_spec_version_is_refused() {
    let (app, store) = test_app().await;
    seed_row(
        &store,
        "AddItem",
        Some("Draft"),
        Some("Draft"),
        "2026-01-01T00:00:00Z",
    )
    .await;
    let mut data = ots_upload("traj-oldspec");
    data["metadata"]["spec_version"] = serde_json::json!("sha256:v1-long-gone");
    store
        .persist_ots_trajectory(&OtsTrajectoryParams {
            trajectory_id: "traj-oldspec",
            tenant: "default",
            agent_id: "agent-1",
            session_id: "session-1",
            outcome: "success",
            turn_count: 1,
            data: &data.to_string(),
        })
        .await
        .expect("persist OTS trajectory");

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "trajectory_id": "traj-oldspec"
            }),
            Some("default"),
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "the run records which spec governed it, and it is not the loaded one"
    );
}

#[tokio::test]
async fn a_run_naming_no_spec_version_is_indeterminate_rather_than_passing() {
    // Nothing says this run executed under the spec that is registered now, and
    // a submit replaces a spec rather than versioning it. Agreeing with
    // whatever is loaded today is not evidence about what ran yesterday.
    let (app, store) = test_app().await;
    seed_row(
        &store,
        "AddItem",
        Some("Draft"),
        Some("Draft"),
        "2026-01-01T00:00:00Z",
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
    assert_eq!(body["report"]["spec_resolution"], "unresolved");
    assert_eq!(body["report"]["verdict"], "indeterminate");
    assert_eq!(
        body["report"]["passed"],
        serde_json::json!(false),
        "a consumer gating on `passed` must not accept a run whose governing spec is unknown"
    );
    assert_eq!(
        body["report"]["evidence_complete"],
        serde_json::json!(false)
    );
    assert_eq!(
        body["report"]["violations"],
        serde_json::json!([]),
        "an unresolved spec is missing evidence, not a disagreement"
    );
}

#[tokio::test]
async fn a_request_cannot_override_the_spec_version_the_trajectory_pins() {
    // The request body is the one input a caller chooses freely. Letting it
    // pick the spec turns "check this run against its spec" into "check this
    // run against whichever spec makes it pass".
    let (app, store) = test_app().await;
    seed_row(
        &store,
        "AddItem",
        Some("Draft"),
        Some("Draft"),
        "2026-01-01T00:00:00Z",
    )
    .await;
    let mut data = ots_upload("traj-pinned");
    data["metadata"]["spec_version"] = serde_json::json!("sha256:v1-long-gone");
    store
        .persist_ots_trajectory(&OtsTrajectoryParams {
            trajectory_id: "traj-pinned",
            tenant: "default",
            agent_id: "agent-1",
            session_id: "session-1",
            outcome: "success",
            turn_count: 1,
            data: &data.to_string(),
        })
        .await
        .expect("persist OTS trajectory");

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "trajectory_id": "traj-pinned",
                // The registered spec: without the pin this would be accepted.
                "spec_version": registered_spec_version(),
            }),
            Some("default"),
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "the recorded version is the run's provenance and a request cannot replace it"
    );
    let detail = response_text(response).await;
    assert!(
        detail.contains("v1-long-gone"),
        "the refusal must name the version the run actually executed under: {detail}"
    );
}

#[tokio::test]
async fn a_request_agreeing_with_the_pinned_spec_version_is_checked() {
    // Same version, one written bare and one `sha256:`-prefixed: two spellings
    // of one spec must not read as a conflict.
    let (app, store) = test_app().await;
    seed_row(
        &store,
        "AddItem",
        Some("Draft"),
        Some("Draft"),
        "2026-01-01T00:00:00Z",
    )
    .await;
    let registered = registered_spec_version();
    let mut data = ots_upload("traj-current");
    data["metadata"]["spec_version"] = serde_json::json!(format!("sha256:{registered}"));
    store
        .persist_ots_trajectory(&OtsTrajectoryParams {
            trajectory_id: "traj-current",
            tenant: "default",
            agent_id: "agent-1",
            session_id: "session-1",
            outcome: "success",
            turn_count: 1,
            data: &data.to_string(),
        })
        .await
        .expect("persist OTS trajectory");

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "trajectory_id": "traj-current",
                "spec_version": registered,
            }),
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    assert_eq!(body["report"]["spec_resolution"], "pinned");
}

/// Seed one legal row and store a trajectory pinning `spec_version`.
async fn app_with_pinned_trajectory(
    trajectory_id: &str,
    spec_version: &str,
) -> (Router, TursoEventStore) {
    let (app, store) = test_app().await;
    seed_row(
        &store,
        "AddItem",
        Some("Draft"),
        Some("Draft"),
        "2026-01-01T00:00:00Z",
    )
    .await;
    let mut data = ots_upload(trajectory_id);
    data["metadata"]["spec_version"] = serde_json::json!(spec_version);
    store
        .persist_ots_trajectory(&OtsTrajectoryParams {
            trajectory_id,
            tenant: "default",
            agent_id: "agent-1",
            session_id: "session-1",
            outcome: "success",
            turn_count: 1,
            data: &data.to_string(),
        })
        .await
        .expect("persist OTS trajectory");
    (app, store)
}

/// The kernel-vocabulary pin katagami stamps: the entity, then a truncated
/// digest.
fn entity_qualified_pin(digest_len: usize) -> String {
    let registered = registered_spec_version();
    format!("Order@sha256:{}", &registered[..digest_len])
}

#[tokio::test]
async fn a_run_pinned_by_entity_and_short_digest_is_checked() {
    // The point of the form: a spec identified by name plus a truncated
    // digest resolves to the same registered spec as the full hash would.
    let (app, _store) =
        app_with_pinned_trajectory("traj-short-pin", &entity_qualified_pin(12)).await;

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "trajectory_id": "traj-short-pin",
            }),
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    let body = json_body(response).await;
    assert_eq!(body["report"]["spec_resolution"], "pinned");
    assert_eq!(
        body["spec_version"],
        registered_spec_version(),
        "the report still names the full digest it was produced against"
    );
}

#[tokio::test]
async fn a_short_pin_and_the_full_hash_are_not_read_as_a_conflict() {
    // The trajectory pins by prefix, the request names the whole hash. Both
    // resolve to the registered spec, so neither contradicts the other.
    let (app, _store) =
        app_with_pinned_trajectory("traj-mixed-forms", &entity_qualified_pin(16)).await;

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "trajectory_id": "traj-mixed-forms",
                "spec_version": registered_spec_version(),
            }),
            Some("default"),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        json_body(response).await["report"]["spec_resolution"],
        "pinned"
    );
}

#[tokio::test]
async fn a_digest_truncated_below_the_floor_is_refused_as_under_specified() {
    // Eleven characters names a family of specs rather than one, so it is
    // refused with what to send instead — not silently resolved, and not
    // reported as the run having executed under a spec that is gone.
    let (app, _store) =
        app_with_pinned_trajectory("traj-too-short", &entity_qualified_pin(11)).await;

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "trajectory_id": "traj-too-short",
            }),
            Some("default"),
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "an under-specified pin is a bad request, not a version conflict"
    );
    let detail = response_text(response).await;
    assert!(
        detail.contains("12") && detail.contains("/observe/specs/Order"),
        "the refusal must say how long a digest to send and where to read it: {detail}"
    );
}

#[tokio::test]
async fn a_pin_naming_another_entity_is_refused() {
    let (app, _store) = app_with_pinned_trajectory(
        "traj-wrong-entity",
        &format!("Invoice@sha256:{}", registered_spec_version()),
    )
    .await;

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "trajectory_id": "traj-wrong-entity",
            }),
            Some("default"),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let detail = response_text(response).await;
    assert!(
        detail.contains("different entity"),
        "the refusal must say the pin belongs to another actor: {detail}"
    );
}

#[tokio::test]
async fn a_short_pin_for_a_spec_that_is_gone_is_still_refused() {
    // Prefix matching widens which spellings name the registered spec. It must
    // not widen which runs are accepted: a prefix of some other digest still
    // has no spec to check against.
    let (app, _store) =
        app_with_pinned_trajectory("traj-gone-prefix", "Order@sha256:0123456789abcdef0123").await;

    let response = app
        .oneshot(admin_post(
            "/api/conformance/check",
            serde_json::json!({
                "entity_type": "Order",
                "session_id": "session-1",
                "trajectory_id": "traj-gone-prefix",
            }),
            Some("default"),
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::CONFLICT,
        "a prefix that names no registered spec is still a version conflict"
    );
}

#[tokio::test]
async fn a_trajectory_with_no_steps_is_not_exported_as_stepless_atif() {
    let (app, store) = test_app().await;
    let data = r#"{"trajectory_id":"traj-empty","version":"0.1.0","metadata":{"task_description":"t","timestamp_start":"2026-01-01T00:00:00Z","agent_id":"a","outcome":"success","human_reviewed":false},"context":{}}"#;
    store
        .persist_ots_trajectory(&OtsTrajectoryParams {
            trajectory_id: "traj-empty",
            tenant: "default",
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
            "/api/ots/trajectories/traj-empty/atif",
            Some("default"),
        ))
        .await
        .unwrap();

    assert_eq!(
        response.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "ATIF v1.7 requires at least one step; a stepless document is not valid ATIF"
    );
}
