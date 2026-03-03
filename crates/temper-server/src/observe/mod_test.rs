use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::sync::Arc;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

use crate::dispatch::AgentContext;
use crate::event_store::ServerEventStore;
use crate::registry::SpecRegistry;

const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

fn test_state_with_registry() -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    let system = ActorSystem::new("test-observe");
    ServerState::from_registry(system, registry)
}

/// Build a test state with a local Turso (SQLite) backend for
/// tests that need persisted data (trajectories, decisions, records).
async fn test_state_with_turso() -> ServerState {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let db_url = format!(
        "file:/tmp/temper-observe-test-{}-{}.db",
        std::process::id(),
        id,
    );
    // Clean up leftover DB from a previous run.
    let _ = std::fs::remove_file(db_url.strip_prefix("file:").unwrap_or(&db_url));
    let turso = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = test_state_with_registry();
    state.event_store = Some(Arc::new(ServerEventStore::Turso(turso)));
    state
}

fn build_test_app() -> Router {
    let state = test_state_with_registry();
    Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state)
}

const ADMIN_MANAGE_POLICIES_POLICY: &str = r#"
permit(
  principal is Admin,
  action == Action::"manage_policies",
  resource is PolicySet
);
"#;

fn install_admin_policy(state: &ServerState) {
    state
        .authz
        .reload_policies(ADMIN_MANAGE_POLICIES_POLICY)
        .expect("admin policy should parse");
}

fn build_app_with_state(state: ServerState) -> Router {
    Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state)
}

#[tokio::test]
async fn test_list_specs_returns_registered_entities() {
    let app = build_test_app();
    let response = app
        .oneshot(Request::get("/observe/specs").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let specs: Vec<SpecSummary> = serde_json::from_slice(&body).unwrap();
    assert!(!specs.is_empty());
    assert_eq!(specs[0].entity_type, "Order");
    assert!(!specs[0].states.is_empty());
    assert!(!specs[0].actions.is_empty());
    // New verification status fields should default to pending
    assert_eq!(specs[0].verification_status, "pending");
    assert!(specs[0].levels_passed.is_none());
    assert!(specs[0].levels_total.is_none());
}

#[tokio::test]
async fn test_get_spec_detail_found() {
    let app = build_test_app();
    let response = app
        .oneshot(
            Request::get("/observe/specs/Order")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let detail: SpecDetail = serde_json::from_slice(&body).unwrap();
    assert_eq!(detail.entity_type, "Order");
    assert!(!detail.states.is_empty());
    assert!(!detail.actions.is_empty());
}

#[tokio::test]
async fn test_get_spec_detail_not_found() {
    let app = build_test_app();
    let response = app
        .oneshot(
            Request::get("/observe/specs/NonExistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_tenant_decisions_accessible_without_auth() {
    let state = test_state_with_registry();
    let app = build_app_with_state(state);

    // Decision list is accessible without auth headers (consistent with
    // other observe endpoints).
    let response = app
        .oneshot(
            Request::get("/api/tenants/default/decisions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn test_tenant_decision_stream_accessible_without_auth() {
    let state = test_state_with_registry();
    let app = build_app_with_state(state);

    // Decision stream is accessible without auth headers.
    let response = app
        .oneshot(
            Request::get("/api/tenants/default/decisions/stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let ct = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/event-stream"),
        "expected SSE content-type, got: {ct}"
    );
}

#[tokio::test]
async fn test_tenant_decision_mutations_require_manage_policies() {
    let state = test_state_with_registry();
    install_admin_policy(&state);
    let app = build_app_with_state(state);

    let deny_approve = app
        .clone()
        .oneshot(
            Request::post("/api/tenants/default/decisions/PD-does-not-exist/approve")
                .header("content-type", "application/json")
                .header("x-temper-principal-id", "cust-1")
                .header("x-temper-principal-kind", "customer")
                .body(Body::from(r#"{"scope":"narrow"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(deny_approve.status(), StatusCode::FORBIDDEN);

    let deny_deny = app
        .oneshot(
            Request::post("/api/tenants/default/decisions/PD-does-not-exist/deny")
                .header("content-type", "application/json")
                .header("x-temper-principal-id", "cust-1")
                .header("x-temper-principal-kind", "customer")
                .body(Body::from(r#"{"decided_by":"cust-1"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(deny_deny.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn test_approve_decision_reload_failure_keeps_pending_and_policies_unchanged() {
    let state = test_state_with_turso().await;
    install_admin_policy(&state);

    let pending = crate::state::PendingDecision::from_denial(
        "default",
        "agent-1",
        "submitOrder",
        "Invalid-Type",
        "order-1",
        serde_json::json!({"id":"order-1"}),
        "test denial",
        None,
    );
    let decision_id = pending.id.clone();
    // Persist decision to Turso (single source of truth).
    state
        .persist_pending_decision(&pending)
        .await
        .expect("persist pending decision to Turso");
    let before_policies = state
        .tenant_policies
        .read()
        .unwrap() // ci-ok: infallible lock
        .clone();

    let app = build_app_with_state(state.clone());
    let response = app
        .oneshot(
            Request::post(format!(
                "/api/tenants/default/decisions/{decision_id}/approve"
            ))
            .header("content-type", "application/json")
            .header("x-temper-principal-id", "admin-1")
            .header("x-temper-principal-kind", "admin")
            .body(Body::from(r#"{"scope":"broad","decided_by":"admin-1"}"#))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

    // Verify decision status unchanged in Turso.
    let turso = state.turso_opt().expect("turso configured");
    let data_str = turso
        .get_pending_decision(&decision_id)
        .await
        .expect("query turso")
        .expect("decision should still exist");
    let decision: crate::state::PendingDecision =
        serde_json::from_str(&data_str).expect("deserialize");
    assert_eq!(decision.status, crate::state::DecisionStatus::Pending);
    assert!(decision.generated_policy.is_none());

    let after_policies = state.tenant_policies.read().unwrap(); // ci-ok: infallible lock
    assert_eq!(*after_policies, before_policies);
}

#[tokio::test]
async fn test_list_entities_empty() {
    let app = build_test_app();
    let response = app
        .oneshot(
            Request::get("/observe/entities")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let entities: Vec<EntityInstanceSummary> = serde_json::from_slice(&body).unwrap();
    // No actors spawned yet, so should be empty
    assert!(entities.is_empty());
}

#[tokio::test]
async fn test_entity_history_returns_events() {
    let state = test_state_with_registry();

    // Dispatch actions to build an event log.
    let r = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            "order-hist-1",
            "AddItem",
            serde_json::json!({"ProductId": "p1"}),
            &AgentContext::default(),
        )
        .await;
    assert!(r.is_ok(), "AddItem failed: {r:?}");

    let r = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            "order-hist-1",
            "SubmitOrder",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await;
    assert!(r.is_ok(), "SubmitOrder failed: {r:?}");

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state);

    let response = app
        .oneshot(
            Request::get("/observe/entities/Order/order-hist-1/history")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert_eq!(json["entity_type"], "Order");
    assert_eq!(json["entity_id"], "order-hist-1");

    let events = json["events"].as_array().expect("events should be array");
    assert_eq!(events.len(), 2);
    assert_eq!(events[0]["action"], "AddItem");
    assert_eq!(events[0]["from_state"], "Draft");
    assert_eq!(events[0]["to_state"], "Draft");
    assert_eq!(events[1]["action"], "SubmitOrder");
    assert_eq!(events[1]["to_state"], "Submitted");
}

#[tokio::test]
async fn test_entity_history_empty_for_unknown() {
    let app = build_test_app();
    let response = app
        .oneshot(
            Request::get("/observe/entities/Order/nonexistent/history")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["entity_type"], "Order");
    assert_eq!(json["entity_id"], "nonexistent");
    let events = json["events"].as_array().expect("events should be array");
    assert!(events.is_empty());
}

// -- Health endpoint tests --

#[tokio::test]
async fn test_health_returns_status() {
    let app = build_test_app();
    let response = app
        .oneshot(Request::get("/observe/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "healthy");
    assert!(json["specs_loaded"].as_u64().is_some());
    assert_eq!(json["event_store"], "none");
}

#[tokio::test]
async fn test_health_counts_entities_and_transitions() {
    let state = test_state_with_registry();

    // Dispatch an action to create an entity and increment metrics.
    let r = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            "health-test-1",
            "AddItem",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await;
    assert!(r.is_ok());

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state);

    let response = app
        .oneshot(Request::get("/observe/health").body(Body::empty()).unwrap())
        .await
        .unwrap();

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["active_entities"], 1);
    assert_eq!(json["transitions_total"], 1);
    assert_eq!(json["errors_total"], 0);
}

// -- Metrics endpoint tests --

#[tokio::test]
async fn test_metrics_returns_prometheus_format() {
    let state = test_state_with_registry();

    // Dispatch a successful and a failed action to populate metrics.
    let _ = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            "metrics-1",
            "AddItem",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await;
    // SubmitOrder with 0 items should fail.
    let _ = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            "metrics-2",
            "SubmitOrder",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await;

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state);

    let response = app
        .oneshot(
            Request::get("/observe/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let ct = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(
        ct.contains("text/plain"),
        "content-type should be text/plain, got: {ct}"
    );

    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let text = std::str::from_utf8(&body).unwrap();
    assert!(
        text.contains("temper_transitions_total"),
        "should contain transitions metric"
    );
    assert!(
        text.contains("temper_active_entities"),
        "should contain active entities metric"
    );
}

// -- Trajectory endpoint tests --

#[tokio::test]
async fn test_trajectories_records_success_and_failure() {
    let state = test_state_with_turso().await;

    // Successful action.
    let r = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            "traj-1",
            "AddItem",
            serde_json::json!({"ProductId": "p1"}),
            &AgentContext::default(),
        )
        .await;
    assert!(r.is_ok());

    // Failed action (SubmitOrder on a brand-new entity with no items guard).
    let _ = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            "traj-2",
            "SubmitOrder",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await;

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state);

    let response = app
        .oneshot(
            Request::get("/observe/trajectories")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();

    assert!(json["total"].as_u64().unwrap() >= 2);
    assert!(json["success_count"].as_u64().unwrap() >= 1);
    assert!(json["error_count"].as_u64().unwrap() >= 1);
    assert!(json["success_rate"].as_f64().unwrap() > 0.0);
    assert!(json["success_rate"].as_f64().unwrap() < 1.0);

    // by_action should have keys for dispatched actions.
    let by_action = json["by_action"].as_object().unwrap();
    assert!(by_action.contains_key("AddItem"));

    // failed_intents should contain at least one entry.
    let failed = json["failed_intents"].as_array().unwrap();
    assert!(!failed.is_empty());
    assert!(failed[0]["error"].is_string());
}

#[tokio::test]
async fn test_trajectories_filters_by_entity_type() {
    let state = test_state_with_turso().await;

    let _ = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            "traj-f1",
            "AddItem",
            serde_json::json!({"ProductId": "p1"}),
            &AgentContext::default(),
        )
        .await;

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state);

    // Filter for entity_type=Order should find our entry.
    let response = app
        .clone()
        .oneshot(
            Request::get("/observe/trajectories?entity_type=Order")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["total"].as_u64().unwrap() >= 1);

    // Filter for non-existent entity_type should return 0.
    let response = app
        .oneshot(
            Request::get("/observe/trajectories?entity_type=Nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["total"], 0);
}

#[tokio::test]
async fn test_trajectories_empty_when_no_actions() {
    let app = build_test_app();

    let response = app
        .oneshot(
            Request::get("/observe/trajectories")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["total"], 0);
    assert_eq!(json["success_count"], 0);
    assert_eq!(json["error_count"], 0);
    assert_eq!(json["success_rate"], 0.0);
    let failed = json["failed_intents"].as_array().unwrap();
    assert!(failed.is_empty());
}

// -- Sentinel endpoint tests --

#[tokio::test]
async fn test_sentinel_check_no_alerts_on_clean_state() {
    let app = build_test_app();

    let response = app
        .oneshot(
            Request::post("/api/evolution/sentinel/check")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["alerts_count"], 0);
    let alerts = json["alerts"].as_array().unwrap();
    assert!(alerts.is_empty());
}

#[tokio::test]
async fn test_sentinel_check_detects_error_spike() {
    let state = test_state_with_registry();

    // Generate high error rate (>10%).
    for i in 0..8 {
        let _ = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                &format!("sentinel-fail-{i}"),
                "SubmitOrder",
                serde_json::json!({}),
                &AgentContext::default(),
            )
            .await;
    }
    for i in 0..2 {
        let _ = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                &format!("sentinel-pass-{i}"),
                "AddItem",
                serde_json::json!({"ProductId": "p1"}),
                &AgentContext::default(),
            )
            .await;
    }

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state);

    let response = app
        .oneshot(
            Request::post("/api/evolution/sentinel/check")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["alerts_count"].as_u64().unwrap() >= 1);

    let alerts = json["alerts"].as_array().unwrap();
    let error_alert = alerts.iter().find(|a| a["rule"] == "error_rate_spike");
    assert!(error_alert.is_some(), "should detect error rate spike");

    let alert = error_alert.unwrap();
    assert!(alert["record_id"].as_str().unwrap().starts_with("O-"));
}

// -- Evolution API endpoint tests --

#[tokio::test]
async fn test_evolution_records_empty() {
    let app = build_test_app();

    let response = app
        .oneshot(
            Request::get("/observe/evolution/records")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["total_observations"], 0);
    assert_eq!(json["total_decisions"], 0);
}

#[tokio::test]
async fn test_evolution_records_after_sentinel() {
    let state = test_state_with_turso().await;

    // Generate errors to trigger sentinel.
    for i in 0..10 {
        let _ = state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                &format!("evo-fail-{i}"),
                "SubmitOrder",
                serde_json::json!({}),
                &AgentContext::default(),
            )
            .await;
    }

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state);

    // Trigger sentinel first.
    let _ = app
        .clone()
        .oneshot(
            Request::post("/api/evolution/sentinel/check")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    // Now check evolution records.
    let response = app
        .oneshot(
            Request::get("/observe/evolution/records")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["total_observations"].as_u64().unwrap() >= 1);
}

#[tokio::test]
async fn test_evolution_get_record_not_found() {
    let state = test_state_with_turso().await;
    let app = build_app_with_state(state);

    let response = app
        .oneshot(
            Request::get("/observe/evolution/records/O-2024-nonexistent")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_evolution_decide_creates_d_record() {
    let state = test_state_with_turso().await;

    // Manually insert an O-Record into Turso.
    let obs = temper_evolution::ObservationRecord {
        header: temper_evolution::RecordHeader {
            id: "O-test-decide".to_string(),
            record_type: temper_evolution::RecordType::Observation,
            timestamp: sim_now(),
            created_by: "test".to_string(),
            derived_from: None,
            status: temper_evolution::RecordStatus::Open,
        },
        source: "test".to_string(),
        classification: temper_evolution::ObservationClass::ErrorRate,
        evidence_query: "test query".to_string(),
        threshold_field: None,
        threshold_value: None,
        observed_value: None,
        context: serde_json::json!({}),
    };
    let data_json = serde_json::to_string(&obs).unwrap();
    state
        .turso_opt()
        .expect("turso configured")
        .insert_evolution_record(
            &obs.header.id,
            "Observation",
            &format!("{:?}", obs.header.status),
            &obs.header.created_by,
            obs.header.derived_from.as_deref(),
            &data_json,
        )
        .await
        .expect("insert O-Record to Turso");

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state);

    // Create a D-Record decision.
    let response = app.clone()
            .oneshot(
                Request::post("/api/evolution/records/O-test-decide/decide")
                    .header("Content-Type", "application/json")
                    .body(Body::from(r#"{"decision":"approved","decided_by":"alice@example.com","rationale":"Looks good"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert!(json["record_id"].as_str().unwrap().starts_with("D-"));
    assert_eq!(json["decision"], "Approved");
    assert_eq!(json["derived_from"], "O-test-decide");
}

#[tokio::test]
async fn test_evolution_decide_not_found() {
    let state = test_state_with_turso().await;
    let app = build_app_with_state(state);

    let response = app
        .oneshot(
            Request::post("/api/evolution/records/O-nonexistent/decide")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    r#"{"decision":"rejected","decided_by":"bob","rationale":"nope"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// -- Workflow endpoint tests --

#[tokio::test]
async fn test_workflows_returns_tenant_data() {
    let app = build_test_app();
    let response = app
        .oneshot(
            Request::get("/observe/workflows")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let workflows = json["workflows"].as_array().unwrap();
    // "default" tenant should appear (but "system" should be filtered out)
    assert!(
        workflows.iter().any(|w| w["tenant"] == "default"),
        "should contain 'default' tenant workflow"
    );
    // Check entity workflow structure
    let default_wf = workflows.iter().find(|w| w["tenant"] == "default").unwrap();
    let entities = default_wf["entities"].as_array().unwrap();
    assert!(!entities.is_empty());
    // Each entity should have 7 steps
    let order_wf = entities.iter().find(|e| e["entity_type"] == "Order");
    assert!(order_wf.is_some(), "should have Order entity workflow");
    let steps = order_wf.unwrap()["steps"].as_array().unwrap();
    assert_eq!(steps.len(), 7, "should have 7 workflow steps");
    assert_eq!(steps[0]["step"], "loaded");
    assert_eq!(steps[6]["step"], "deployed");
}

// -- Load-dir endpoint tests --

#[tokio::test]
async fn test_load_dir_registers_specs() {
    let system = ActorSystem::new("test-load-dir");
    let registry = SpecRegistry::new();
    let state = ServerState::from_registry(system, registry);

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state.clone());

    // Use the test-fixtures/specs directory which has valid specs
    let specs_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-fixtures/specs");

    let body = serde_json::json!({
        "tenant": "test-tenant",
        "specs_dir": specs_dir.to_str().unwrap(),
    });

    let response = app
        .oneshot(
            Request::post("/api/specs/load-dir")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    // Response is NDJSON — parse each line
    let body = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    let lines: Vec<serde_json::Value> = body_str
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    // First line: specs_loaded
    assert_eq!(lines[0]["type"], "specs_loaded");
    assert_eq!(lines[0]["tenant"], "test-tenant");
    let entities = lines[0]["entities"].as_array().unwrap();
    assert!(
        !entities.is_empty(),
        "should have loaded at least one entity"
    );

    // Last line: summary
    let summary = lines.last().unwrap();
    assert_eq!(summary["type"], "summary");
    assert_eq!(summary["tenant"], "test-tenant");

    // Verify specs are in the registry
    let registry = state.registry.read().unwrap();
    let tenant_id: temper_runtime::tenant::TenantId = "test-tenant".into();
    let entity_types = registry.entity_types(&tenant_id);
    assert!(
        !entity_types.is_empty(),
        "registry should have entity types for test-tenant"
    );
}

#[tokio::test]
async fn test_load_dir_missing_dir_returns_error() {
    let system = ActorSystem::new("test-load-dir-missing");
    let registry = SpecRegistry::new();
    let state = ServerState::from_registry(system, registry);

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state);

    let body = serde_json::json!({
        "tenant": "test-tenant",
        "specs_dir": "/nonexistent/path/to/specs",
    });

    let response = app
        .oneshot(
            Request::post("/api/specs/load-dir")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_load_dir_lint_error_aborts_registration() {
    let system = ActorSystem::new("test-load-dir-lint-error");
    let registry = SpecRegistry::new();
    let state = ServerState::from_registry(system, registry);

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state.clone());

    let temp_specs =
        std::env::temp_dir().join(format!("temper-load-dir-lint-{}", uuid::Uuid::new_v4())); // determinism-ok: test-only temp dir
    std::fs::create_dir_all(&temp_specs).expect("create temp specs dir"); // determinism-ok: test-only
    std::fs::write(
        // determinism-ok: test-only
        temp_specs.join("model.csdl.xml"),
        include_str!("../../../../test-fixtures/specs/model.csdl.xml"),
    )
    .expect("write csdl");
    std::fs::write(
        // determinism-ok: test-only
        temp_specs.join("order.ioa.toml"),
        r#"
[automaton]
name = "Order"
states = ["Draft", "Done"]
initial = "Draft"

[[action]]
name = "Complete"
from = ["Draft"]
to = "Done"
effect = "set phantom true"
"#,
    )
    .expect("write ioa");

    let body = serde_json::json!({
        "tenant": "lint-tenant",
        "specs_dir": temp_specs.to_str().unwrap(),
    });

    let response = app
        .oneshot(
            Request::post("/api/specs/load-dir")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    let _ = std::fs::remove_dir_all(&temp_specs); // determinism-ok: test-only

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let body_str = std::str::from_utf8(&body).unwrap();
    let lines: Vec<serde_json::Value> = body_str
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();

    assert_eq!(lines[0]["type"], "specs_loaded");
    assert!(lines.iter().any(|l| l["type"] == "lint_error"));
    assert!(!lines.iter().any(|l| l["type"] == "verification_started"));

    let registry = state.registry.read().unwrap();
    let tenant_id: temper_runtime::tenant::TenantId = "lint-tenant".into();
    assert!(
        registry.get_tenant(&tenant_id).is_none(),
        "tenant should not be registered when lint errors exist"
    );
}

#[tokio::test]
async fn test_load_dir_emits_design_time_events() {
    let db_url = format!(
        "file:/tmp/temper-design-time-test-{}.db",
        std::process::id(),
    );
    let turso = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let system = ActorSystem::new("test-load-dir-events");
    let registry = SpecRegistry::new();
    let mut state = ServerState::from_registry(system, registry);
    state.event_store = Some(Arc::new(ServerEventStore::Turso(turso)));

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state.clone());

    let specs_dir =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../test-fixtures/specs");

    let body = serde_json::json!({
        "tenant": "event-tenant",
        "specs_dir": specs_dir.to_str().unwrap(),
    });

    let response = app
        .oneshot(
            Request::post("/api/specs/load-dir")
                .header("Content-Type", "application/json")
                .body(Body::from(serde_json::to_string(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    // Consume entire body to wait for verification to complete
    let _ = axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
        .await
        .unwrap();

    // Check that design-time events were persisted to Turso.
    let turso = state.turso_opt().expect("turso configured");
    let events = turso
        .list_design_time_events(None, 1000)
        .await
        .expect("query design-time events from Turso");
    assert!(!events.is_empty(), "design-time events should be in Turso");

    // Should have spec_loaded, verify_started, and verify_done events
    let loaded_events: Vec<_> = events.iter().filter(|e| e.kind == "spec_loaded").collect();
    assert!(!loaded_events.is_empty(), "should have spec_loaded events");

    let started_events: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "verify_started")
        .collect();
    assert!(
        !started_events.is_empty(),
        "should have verify_started events"
    );

    let done_events: Vec<_> = events.iter().filter(|e| e.kind == "verify_done").collect();
    assert!(!done_events.is_empty(), "should have verify_done events");
}

#[tokio::test]
async fn test_evolution_insights_empty() {
    let app = build_test_app();

    let response = app
        .oneshot(
            Request::get("/observe/evolution/insights")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["total"], 0);
    let insights = json["insights"].as_array().unwrap();
    assert!(insights.is_empty());
}
