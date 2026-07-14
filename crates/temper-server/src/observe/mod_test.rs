use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::sim_now;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Record};
use tracing::{Id, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;
use tracing_subscriber::registry::LookupSpan;

use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::secrets::vault::SecretsVault;
use crate::state::{TrajectoryEntry, TrajectorySource};
use crate::storage::StorageStack;

const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");
const VALID_EMPTY_WASM: &[u8] = b"\0asm\x01\0\0\0";

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
    let data_dir = std::path::PathBuf::from(format!(
        "/tmp/temper-observe-test-{}-{}-data",
        std::process::id(),
        id,
    ));
    // Clean up leftover DB from a previous run.
    let _ = std::fs::remove_file(db_url.strip_prefix("file:").unwrap_or(&db_url));
    let _ = std::fs::remove_dir_all(&data_dir);
    let turso = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = test_state_with_registry();
    state.data_dir = data_dir;
    state.set_storage_stack(StorageStack::from_turso(turso));
    state
}

fn feature_request_db_url() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    format!(
        "file:/tmp/temper-feature-request-test-{}-{}.db",
        std::process::id(),
        id,
    )
}

async fn test_state_with_feature_request_runtime_at(db_url: &str) -> ServerState {
    let order_csdl = parse_csdl(CSDL_XML).expect("order CSDL should parse");
    let platform_csdl_source = include_str!("../../../temper-platform/src/specs/model.csdl.xml");
    let platform_csdl = parse_csdl(platform_csdl_source).expect("platform CSDL should parse");
    let feature_request_ioa =
        include_str!("../../../temper-platform/src/specs/FeatureRequest.ioa.toml");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        "default",
        order_csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    registry.register_tenant(
        "temper-system",
        platform_csdl,
        platform_csdl_source.to_string(),
        &[("FeatureRequest", feature_request_ioa)],
    );

    let turso = TursoEventStore::new(db_url, None)
        .await
        .expect("create feature-request Turso db");
    let system = ActorSystem::new("test-feature-request-runtime");
    let mut state = ServerState::from_registry(system, registry);
    state.set_storage_stack(StorageStack::from_turso(turso));
    state
}

async fn test_state_with_feature_request_runtime() -> ServerState {
    let db_url = feature_request_db_url();
    let _ = std::fs::remove_file(db_url.strip_prefix("file:").unwrap_or(&db_url));
    test_state_with_feature_request_runtime_at(&db_url).await
}

fn build_test_app() -> Router {
    let state = test_state_with_registry();
    Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state)
}

/// Build a GET request with admin auth headers for observe endpoints.
///
/// Uses "admin" principal kind — "system" is no longer accepted from headers
/// (only via `SecurityContext::system()` on internal paths).
fn system_get(uri: &str) -> Request<Body> {
    Request::get(uri)
        .header("X-Temper-Principal-Kind", "admin")
        .body(Body::empty())
        .unwrap()
}

async fn observe_json(app: Router, uri: &str) -> serde_json::Value {
    let response = app.oneshot(system_get(uri)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&body).unwrap()
}

async fn wait_for_trajectory_total(app: Router, uri: &str, min_total: u64) -> serde_json::Value {
    // Trajectory persistence runs through the bounded outbox drainer
    // (ADR-0067), which spawns each persist concurrently. Poll briefly to let
    // the drainer flush this test's entries.
    for _ in 0..100 {
        let json = observe_json(app.clone(), uri).await;
        if json["total"].as_u64().unwrap_or(0) >= min_total {
            return json;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    observe_json(app, uri).await
}

// -- Projection correctness probe tests --

#[tokio::test]
async fn projection_replay_parity_endpoint_reports_clean_bounded_scope() {
    let state = test_state_with_turso().await;
    let tenant = TenantId::default();
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            "projection-probe-clean-a",
            serde_json::json!({"Title": "Projection Probe Clean"}),
        )
        .await
        .expect("create projected entity");
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            "projection-probe-clean-b",
            serde_json::json!({"Title": "Projection Probe Clean Extra"}),
        )
        .await
        .expect("create second projected entity");

    let app = build_app_with_state(state);
    let json = observe_json(
        app,
        "/observe/projections/replay-parity?entity_type=Order&limit=1",
    )
    .await;

    assert_eq!(json["kind"], "query_projection_replay_parity");
    assert_eq!(json["clean"], true);
    assert_eq!(json["limit"], 1);
    assert_eq!(json["report"]["tenant"], "default");
    assert_eq!(json["report"]["entity_type"], "Order");
    assert_eq!(json["report"]["entity_limit"], 1);
    assert_eq!(json["report"]["checked"], 1);
    assert_eq!(json["report"]["matched"], 1);
    assert_eq!(json["report"]["drifted"], 0);
}

#[tokio::test]
async fn projection_replay_parity_endpoint_reports_projection_drift() {
    let state = test_state_with_turso().await;
    let tenant = TenantId::default();
    let entity_id = "projection-probe-drift";
    state
        .get_or_create_tenant_entity(
            &tenant,
            "Order",
            entity_id,
            serde_json::json!({"Title": "Projection Probe Original"}),
        )
        .await
        .expect("create projected entity");
    let actor_state = state
        .get_tenant_entity_state(&tenant, "Order", entity_id)
        .await
        .expect("load authoritative entity");
    let projection_state = state.query_projection_state(&actor_state.state);
    state
        .query_plane_store()
        .expect("query-plane store")
        .upsert_projection(
            tenant.as_str(),
            "Order",
            entity_id,
            &actor_state.state.status,
            &serde_json::json!({"Title": "Projection Probe Drift"}),
            &projection_state,
            actor_state.state.sequence_nr,
        )
        .await
        .expect("inject projection drift");

    let app = build_app_with_state(state);
    let json = observe_json(
        app,
        "/observe/projections/replay-parity?entity_type=Order&limit=10",
    )
    .await;

    assert_eq!(json["clean"], false);
    assert_eq!(json["report"]["checked"], 1);
    assert_eq!(json["report"]["matched"], 0);
    assert_eq!(json["report"]["drifted"], 1);
    assert_eq!(json["report"]["drift_examples"][0]["entity_id"], entity_id);
    assert_eq!(json["report"]["drift_examples"][0]["drift_kind"], "fields");
}

/// Build a POST request with admin auth headers for observe endpoints.
fn system_post(uri: &str, body: &str) -> Request<Body> {
    Request::post(uri)
        .header("Content-Type", "application/json")
        .header("X-Temper-Principal-Kind", "admin")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Build a POST request with admin auth headers.
#[allow(dead_code)]
fn admin_post(uri: &str, body: &str) -> Request<Body> {
    Request::post(uri)
        .header("Content-Type", "application/json")
        .header("X-Temper-Principal-Kind", "admin")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn agent_post(uri: &str, body: impl Into<Body>) -> Request<Body> {
    Request::post(uri)
        .header("X-Tenant-Id", "default")
        .header("X-Temper-Principal-Id", "agent-1")
        .header("X-Temper-Principal-Kind", "agent")
        .header("X-Temper-Agent-Type", "swe")
        .header("X-Temper-Ctx-SessionId", "session-1")
        .body(body.into())
        .unwrap()
}

fn agent_get(uri: &str) -> Request<Body> {
    Request::get(uri)
        .header("X-Tenant-Id", "default")
        .header("X-Temper-Principal-Id", "agent-1")
        .header("X-Temper-Principal-Kind", "agent")
        .header("X-Temper-Agent-Type", "swe")
        .header("X-Temper-Ctx-SessionId", "session-1")
        .body(Body::empty())
        .unwrap()
}

const ADMIN_MANAGE_POLICIES_POLICY: &str = r#"
permit(
  principal is Admin,
  action == Action::"manage_policies",
  resource is PolicySet
);
"#;

const ADMIN_SUBMIT_SPECS_POLICY: &str = r#"
permit(
  principal is Admin,
  action == Action::"submit_specs",
  resource is SpecRegistry
);
"#;

fn install_admin_policy(state: &ServerState) {
    state
        .authz
        .reload_policies(ADMIN_MANAGE_POLICIES_POLICY)
        .expect("admin policy should parse");
}

fn install_admin_submit_specs_policy(state: &ServerState) {
    state
        .authz
        .reload_policies(ADMIN_SUBMIT_SPECS_POLICY)
        .expect("submit_specs policy should parse");
}

fn build_app_with_state(state: ServerState) -> Router {
    Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state)
}

#[derive(Clone, Default)]
struct CapturedSpans {
    spans: Arc<Mutex<Vec<CapturedSpan>>>,
}

#[derive(Clone, Debug)]
struct CapturedSpan {
    name: String,
    fields: BTreeMap<String, String>,
}

#[derive(Clone, Copy)]
struct CapturedSpanIndex(usize);

impl<S> Layer<S> for CapturedSpans
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut visitor = CapturedFieldVisitor::default();
        attrs.record(&mut visitor);

        let mut spans = self.spans.lock().expect("span capture lock poisoned");
        let index = spans.len();
        spans.push(CapturedSpan {
            name: attrs.metadata().name().to_string(),
            fields: visitor.fields,
        });

        if let Some(span) = ctx.span(id) {
            span.extensions_mut().insert(CapturedSpanIndex(index));
        }
    }

    fn on_record(&self, id: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        let Some(span) = ctx.span(id) else {
            return;
        };
        let Some(index) = span.extensions().get::<CapturedSpanIndex>().copied() else {
            return;
        };

        let mut visitor = CapturedFieldVisitor::default();
        values.record(&mut visitor);
        if let Some(captured) = self
            .spans
            .lock()
            .expect("span capture lock poisoned")
            .get_mut(index.0)
        {
            captured.fields.extend(visitor.fields);
        }
    }
}

#[derive(Default)]
struct CapturedFieldVisitor {
    fields: BTreeMap<String, String>,
}

impl Visit for CapturedFieldVisitor {
    fn record_bool(&mut self, field: &Field, value: bool) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.fields
            .insert(field.name().to_string(), value.to_string());
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.fields
            .insert(field.name().to_string(), format!("{value:?}"));
    }
}

#[tokio::test]
async fn wasm_upload_denial_creates_pending_decision_for_module_resource() {
    let state = test_state_with_turso().await;
    install_admin_policy(&state);
    let app = build_app_with_state(state.clone());

    let response = app
        .oneshot(agent_post(
            "/api/wasm/modules/git_upload_pack",
            Body::from(VALID_EMPTY_WASM.to_vec()),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("denial response JSON");
    let decision_id = json["decision_id"].as_str().expect("top-level decision_id");

    let turso = state.platform_turso_store().expect("turso configured");
    let data_str = turso
        .get_pending_decision(decision_id)
        .await
        .expect("query decision")
        .expect("decision should be persisted");
    let decision: crate::state::PendingDecision =
        serde_json::from_str(&data_str).expect("deserialize pending decision");

    assert_eq!(decision.tenant, "default");
    assert_eq!(decision.agent_id, "agent-1");
    assert_eq!(decision.action, "manage_wasm");
    assert_eq!(decision.resource_type, "WasmModule");
    assert_eq!(decision.resource_id, "git_upload_pack");
    assert_eq!(decision.resource_attrs["id"], "git_upload_pack");
    assert_eq!(decision.module_name.as_deref(), Some("git_upload_pack"));
    assert_eq!(decision.session_id.as_deref(), Some("session-1"));
    assert_eq!(decision.status, crate::state::DecisionStatus::Pending);
}

#[tokio::test]
async fn wasm_delete_denial_creates_pending_decision_for_module_resource() {
    let state = test_state_with_turso().await;
    install_admin_policy(&state);
    let app = build_app_with_state(state.clone());

    let response = app
        .oneshot(
            Request::delete("/api/wasm/modules/git_receive_pack")
                .header("X-Tenant-Id", "default")
                .header("X-Temper-Principal-Id", "agent-1")
                .header("X-Temper-Principal-Kind", "agent")
                .header("X-Temper-Agent-Type", "swe")
                .header("X-Temper-Ctx-SessionId", "session-1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("denial response JSON");
    let decision_id = json["decision_id"].as_str().expect("top-level decision_id");

    let turso = state.platform_turso_store().expect("turso configured");
    let data_str = turso
        .get_pending_decision(decision_id)
        .await
        .expect("query decision")
        .expect("decision should be persisted");
    let decision: crate::state::PendingDecision =
        serde_json::from_str(&data_str).expect("deserialize pending decision");
    assert_eq!(decision.action, "manage_wasm");
    assert_eq!(decision.resource_type, "WasmModule");
    assert_eq!(decision.resource_id, "git_receive_pack");
    assert_eq!(decision.module_name.as_deref(), Some("git_receive_pack"));
}

#[tokio::test]
async fn wasm_upload_accepts_json_base64_body() {
    use base64::Engine;

    let state = test_state_with_turso().await;
    let app = build_app_with_state(state);
    let payload = serde_json::json!({
        "wasm_base64": base64::engine::general_purpose::STANDARD.encode(VALID_EMPTY_WASM),
    });

    let response = app
        .oneshot(admin_post(
            "/api/wasm/modules/base64_module",
            &payload.to_string(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("upload response JSON");
    assert_eq!(json["module_name"], "base64_module");
    assert_eq!(json["size_bytes"], VALID_EMPTY_WASM.len());
}

#[tokio::test]
async fn approved_wasm_upload_decision_allows_agent_retry() {
    let state = test_state_with_turso().await;
    install_admin_policy(&state);
    let app = build_app_with_state(state);

    let denied = app
        .clone()
        .oneshot(agent_post(
            "/api/wasm/modules/git_upload_pack",
            Body::from(VALID_EMPTY_WASM.to_vec()),
        ))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);
    let denied_body = axum::body::to_bytes(denied.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let denied_json: serde_json::Value =
        serde_json::from_slice(&denied_body).expect("denial response JSON");
    let decision_id = denied_json["decision_id"]
        .as_str()
        .expect("top-level decision_id");

    let approved = app
        .clone()
        .oneshot(
            Request::post(format!(
                "/api/tenants/default/decisions/{decision_id}/approve"
            ))
            .header("content-type", "application/json")
            .header("x-temper-principal-id", "admin-1")
            .header("x-temper-principal-kind", "admin")
            .body(Body::from(r#"{"scope":{"principal":"this_agent","action":"this_action","resource":"this_resource","duration":"always"},"decided_by":"admin-1"}"#))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(approved.status(), StatusCode::OK);

    let retried = app
        .oneshot(agent_post(
            "/api/wasm/modules/git_upload_pack",
            Body::from(VALID_EMPTY_WASM.to_vec()),
        ))
        .await
        .unwrap();
    assert_eq!(retried.status(), StatusCode::OK);
    let retried_body = axum::body::to_bytes(retried.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let retried_json: serde_json::Value =
        serde_json::from_slice(&retried_body).expect("retry response JSON");
    assert_eq!(retried_json["module_name"], "git_upload_pack");
}

#[tokio::test]
async fn tenant_decision_lookup_returns_known_decision_by_id() {
    let state = test_state_with_turso().await;
    install_admin_policy(&state);
    let pending = crate::state::PendingDecision::from_denial(
        "default",
        "agent-1",
        "manage_wasm",
        "WasmModule",
        "git_upload_pack",
        serde_json::json!({"id":"git_upload_pack"}),
        "test denial",
        Some("git_upload_pack".to_string()),
    );
    let decision_id = pending.id.clone();
    state
        .persist_pending_decision(&pending)
        .await
        .expect("persist pending decision");
    let app = build_app_with_state(state);

    let response = app
        .oneshot(
            Request::get(format!("/api/tenants/default/decisions/{decision_id}"))
                .header("X-Temper-Principal-Kind", "admin")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("decision JSON");
    assert_eq!(json["id"], decision_id);
    assert_eq!(json["resource_type"], "WasmModule");
    assert_eq!(json["resource_id"], "git_upload_pack");
}

#[tokio::test]
async fn tenant_decision_list_allows_agent_to_read_owned_pending_decisions() {
    let state = test_state_with_turso().await;
    install_admin_policy(&state);

    let mut owned = crate::state::PendingDecision::from_denial(
        "default",
        "agent-1",
        "manage_wasm",
        "WasmModule",
        "git_upload_pack",
        serde_json::json!({"id":"git_upload_pack"}),
        "test denial",
        Some("git_upload_pack".to_string()),
    );
    owned.session_id = Some("session-1".to_string());
    let mut other = crate::state::PendingDecision::from_denial(
        "default",
        "agent-2",
        "manage_wasm",
        "WasmModule",
        "git_receive_pack",
        serde_json::json!({"id":"git_receive_pack"}),
        "test denial",
        Some("git_receive_pack".to_string()),
    );
    other.session_id = Some("session-2".to_string());
    let owned_id = owned.id.clone();
    state
        .persist_pending_decision(&owned)
        .await
        .expect("persist owned pending decision");
    state
        .persist_pending_decision(&other)
        .await
        .expect("persist other pending decision");
    let app = build_app_with_state(state);

    let response = app
        .oneshot(agent_get("/api/tenants/default/decisions?status=pending"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).expect("decision list JSON");
    assert_eq!(json["total"], 1);
    assert_eq!(json["pending_count"], 1);
    assert_eq!(json["decisions"][0]["id"], owned_id);
    assert_eq!(json["decisions"][0]["agent_id"], "agent-1");
    assert_eq!(json["decisions"][0]["session_id"], "session-1");
}

#[tokio::test]
async fn batch_file_text_read_returns_projected_file_contents_in_request_order() {
    let state = test_state_with_turso().await;
    let tenant = "default";
    let turso = state
        .turso_store_for_tenant(tenant)
        .await
        .expect("tenant turso store");

    turso
        .upsert_query_projection(
            tenant,
            "File",
            "file-a",
            "Ready",
            &serde_json::json!({
                "content_hash": "sha256:file-a",
                "mime_type": "application/json",
                "has_content": true,
            }),
            1,
        )
        .await
        .expect("upsert file-a projection");
    turso
        .upsert_query_projection(
            tenant,
            "File",
            "file-b",
            "Created",
            &serde_json::json!({
                "content_hash": "",
                "mime_type": "text/plain",
                "has_content": false,
            }),
            1,
        )
        .await
        .expect("upsert file-b projection");
    turso
        .put_blob("temper-fs/sha256:file-a", b"{\"msg\":\"hello\"}")
        .await
        .expect("persist blob");

    let app = build_app_with_state(state);
    let response = app
        .oneshot(system_post(
            "/api/files/read-text-batch",
            r#"{"file_ids":["file-a","file-b","missing"]}"#,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let files = json["files"].as_array().expect("files array");
    assert_eq!(files.len(), 3);

    assert_eq!(files[0]["file_id"], "file-a");
    assert_eq!(files[0]["found"], true);
    assert_eq!(files[0]["content_hash"], "sha256:file-a");
    assert_eq!(files[0]["mime_type"], "application/json");
    assert_eq!(files[0]["text"], "{\"msg\":\"hello\"}");

    assert_eq!(files[1]["file_id"], "file-b");
    assert_eq!(files[1]["found"], true);
    assert_eq!(files[1]["text"], "");

    assert_eq!(files[2]["file_id"], "missing");
    assert_eq!(files[2]["found"], false);
    assert_eq!(files[2]["text"], "");
}

#[tokio::test]
async fn batch_file_version_text_read_returns_immutable_version_contents_in_request_order() {
    let state = test_state_with_turso().await;
    let tenant = "default";
    let turso = state
        .turso_store_for_tenant(tenant)
        .await
        .expect("tenant turso store");

    turso
        .upsert_query_projection(
            tenant,
            "FileVersion",
            "ver-a",
            "Current",
            &serde_json::json!({
                "content_hash": "sha256:ver-a",
                "mime_type": "application/json",
            }),
            1,
        )
        .await
        .expect("upsert ver-a projection");
    turso
        .upsert_query_projection(
            tenant,
            "FileVersion",
            "ver-b",
            "Superseded",
            &serde_json::json!({
                "content_hash": "",
                "mime_type": "text/plain",
            }),
            1,
        )
        .await
        .expect("upsert ver-b projection");
    turso
        .put_blob("temper-fs/sha256:ver-a", b"{\"msg\":\"immutable\"}")
        .await
        .expect("persist blob");

    let app = build_app_with_state(state);
    let response = app
        .oneshot(system_post(
            "/api/files/read-version-text-batch",
            r#"{"file_version_ids":["ver-a","ver-b","missing"]}"#,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let files = json["files"].as_array().expect("files array");
    assert_eq!(files.len(), 3);

    assert_eq!(files[0]["file_version_id"], "ver-a");
    assert_eq!(files[0]["found"], true);
    assert_eq!(files[0]["content_hash"], "sha256:ver-a");
    assert_eq!(files[0]["mime_type"], "application/json");
    assert_eq!(files[0]["text"], "{\"msg\":\"immutable\"}");

    assert_eq!(files[1]["file_version_id"], "ver-b");
    assert_eq!(files[1]["found"], true);
    assert_eq!(files[1]["text"], "");

    assert_eq!(files[2]["file_version_id"], "missing");
    assert_eq!(files[2]["found"], false);
    assert_eq!(files[2]["text"], "");
}

#[tokio::test]
async fn batch_file_version_text_read_uses_local_store_for_internal_blob_endpoint() {
    let mut state = test_state_with_turso().await;
    let tenant = "default";
    let turso = state
        .turso_store_for_tenant(tenant)
        .await
        .expect("tenant turso store");

    turso
        .upsert_query_projection(
            tenant,
            "FileVersion",
            "ver-local",
            "Current",
            &serde_json::json!({
                "content_hash": "sha256:ver-local",
                "mime_type": "text/plain",
            }),
            1,
        )
        .await
        .expect("upsert ver-local projection");
    turso
        .put_blob("temper-fs/sha256:ver-local", b"local-fast-path")
        .await
        .expect("persist blob");

    let vault = SecretsVault::new(&[7u8; 32]);
    vault
        .cache_secret(
            tenant,
            "blob_endpoint",
            "http://127.0.0.1:3474/_internal/blobs".to_string(),
        )
        .expect("cache blob endpoint");
    state.secrets_vault = Some(Arc::new(vault));

    let app = build_app_with_state(state);
    let response = app
        .oneshot(system_post(
            "/api/files/read-version-text-batch",
            r#"{"file_version_ids":["ver-local"]}"#,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let files = json["files"].as_array().expect("files array");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0]["file_version_id"], "ver-local");
    assert_eq!(files[0]["found"], true);
    assert_eq!(files[0]["text"], "local-fast-path");
}

#[tokio::test]
async fn test_list_specs_returns_registered_entities() {
    let app = build_test_app();
    let response = app.oneshot(system_get("/observe/specs")).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let wrapper: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let specs: Vec<SpecSummary> = serde_json::from_value(wrapper["specs"].clone()).unwrap();
    assert!(!specs.is_empty());
    assert_eq!(specs[0].entity_type, "Order");
    assert!(!specs[0].states.is_empty());
    assert!(!specs[0].actions.is_empty());
    // New verification status fields should default to pending
    assert_eq!(specs[0].verification_status, "pending");
    assert!(specs[0].levels_passed.is_none());
    assert!(specs[0].levels_total.is_none());
    assert_eq!(wrapper["total"], specs.len());
}

#[tokio::test]
async fn test_get_spec_detail_found() {
    let app = build_test_app();
    let response = app
        .oneshot(system_get("/observe/specs/Order"))
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
        .oneshot(system_get("/observe/specs/NonExistent"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_load_inline_supports_nested_paths() {
    let state = test_state_with_registry();
    install_admin_submit_specs_policy(&state);
    let app = build_app_with_state(state.clone());

    let response = app
        .oneshot(system_post(
            "/api/specs/load-inline",
            &serde_json::json!({
                "tenant": "nested-inline",
                "specs": {
                    "InlineProbe/model.csdl.xml": CSDL_XML,
                    "InlineProbe/order.ioa.toml": ORDER_IOA
                }
            })
            .to_string(),
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);

    let registry = state.registry.read().unwrap();
    let tenant = TenantId::new("nested-inline");
    let spec = registry
        .get_spec(&tenant, "Order")
        .expect("nested inline load should register Order");
    assert_eq!(spec.automaton.automaton.name, "Order");
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
                .body(Body::from(r#"{"scope":{"principal":"this_agent","action":"this_action","resource":"this_resource","duration":"always"}}"#))
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
            .body(Body::from(r#"{"scope":{"principal":"this_agent","action":"this_action","resource":"this_resource","duration":"always"},"decided_by":"admin-1"}"#))
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Verify decision status unchanged in Turso.
    let turso = state.platform_turso_store().expect("turso configured");
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
    let response = app.oneshot(system_get("/observe/entities")).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let wrapper: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entities: Vec<EntityInstanceSummary> =
        serde_json::from_value(wrapper["entities"].clone()).unwrap();
    // No actors spawned yet, so should be empty
    assert!(entities.is_empty());
    assert_eq!(wrapper["total"], 0);
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
        .oneshot(system_get("/observe/entities/Order/order-hist-1/history"))
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
        .oneshot(system_get("/observe/entities/Order/nonexistent/history"))
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

#[tokio::test]
async fn test_entity_wait_returns_terminal_state() {
    let state = test_state_with_registry();
    let tenant = TenantId::default();
    let create = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "order-wait-1",
            "AddItem",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await;
    assert!(create.is_ok(), "AddItem failed: {create:?}");

    let delayed_state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        delayed_state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "order-wait-1",
                "SubmitOrder",
                serde_json::json!({}),
                &AgentContext::default(),
            )
            .await
            .expect("SubmitOrder should succeed");
    });

    let app = build_app_with_state(state);
    let response = app
        .oneshot(system_get(
            "/observe/entities/Order/order-wait-1/wait?statuses=Submitted&timeout_ms=1000&poll_ms=10",
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "Submitted");
    assert_eq!(json["timed_out"], false);
}

#[test]
fn test_wait_span_identity_recorder_sets_datadog_filter_fields() {
    use tracing_subscriber::prelude::*;

    let captured = CapturedSpans::default();
    let subscriber = tracing_subscriber::registry().with(captured.clone());
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);

    let span = tracing::info_span!(
        "handle_wait_for_entity_state",
        otel.name = "GET /observe/entities/{entity_type}/{entity_id}/wait",
        tenant = tracing::field::Empty,
        entity_type = tracing::field::Empty,
        entity_id = tracing::field::Empty,
        wait.wake_reason = tracing::field::Empty
    );
    let _span_guard = span.enter();
    entities::record_wait_span_identity(&TenantId::default(), "Order", "order-wait-span-1");
    tracing::Span::current().record("wait.wake_reason", "initial_state");

    let spans = captured.spans.lock().expect("span capture lock poisoned");
    let wait_span = spans
        .iter()
        .find(|span| span.name == "handle_wait_for_entity_state")
        .expect("wait route span should be captured");

    assert_eq!(
        wait_span.fields.get("otel.name").map(String::as_str),
        Some("GET /observe/entities/{entity_type}/{entity_id}/wait")
    );
    assert_eq!(
        wait_span.fields.get("tenant").map(String::as_str),
        Some("default")
    );
    assert_eq!(
        wait_span.fields.get("entity_type").map(String::as_str),
        Some("Order")
    );
    assert_eq!(
        wait_span.fields.get("entity_id").map(String::as_str),
        Some("order-wait-span-1")
    );
    assert_eq!(
        wait_span.fields.get("wait.wake_reason").map(String::as_str),
        Some("initial_state")
    );
}

#[tokio::test]
async fn test_entity_wait_wakes_from_state_change_event_before_poll_interval() {
    let state = test_state_with_registry();
    let tenant = TenantId::default();
    let create = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "order-wait-event-1",
            "AddItem",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await;
    assert!(create.is_ok(), "AddItem failed: {create:?}");

    let delayed_state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        delayed_state
            .dispatch_tenant_action(
                &TenantId::default(),
                "Order",
                "order-wait-event-1",
                "SubmitOrder",
                serde_json::json!({}),
                &AgentContext::default(),
            )
            .await
            .expect("SubmitOrder should succeed");
    });

    let app = build_app_with_state(state);
    let response = tokio::time::timeout(
        Duration::from_millis(1500),
        app.oneshot(system_get(
            "/observe/entities/Order/order-wait-event-1/wait?statuses=Submitted&timeout_ms=3000&poll_ms=5000",
        )),
    )
    .await
    .expect("event-driven wait should return before the fallback poll interval")
    .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "Submitted");
    assert_eq!(json["timed_out"], false);
}

#[tokio::test]
async fn test_entity_wait_times_out_with_current_state() {
    let state = test_state_with_registry();
    let tenant = TenantId::default();
    let create = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "order-wait-timeout",
            "AddItem",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await;
    assert!(create.is_ok(), "AddItem failed: {create:?}");

    let app = build_app_with_state(state);
    let response = app
        .oneshot(system_get(
            "/observe/entities/Order/order-wait-timeout/wait?statuses=Submitted&timeout_ms=50&poll_ms=10",
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["status"], "Draft");
    assert_eq!(json["timed_out"], true);
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
    assert_eq!(json["active_actors"], 1);
    assert_eq!(json["indexed_entities"], 1);
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
        text.contains("temper_indexed_entities"),
        "should contain indexed entities metric"
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

    let json = wait_for_trajectory_total(app, "/observe/trajectories", 2).await;

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
    let json =
        wait_for_trajectory_total(app.clone(), "/observe/trajectories?entity_type=Order", 1).await;
    assert!(json["total"].as_u64().unwrap() >= 1);

    // Filter for non-existent entity_type should return 0.
    let json = observe_json(app, "/observe/trajectories?entity_type=Nonexistent").await;
    assert_eq!(json["total"], 0);
}

#[tokio::test]
async fn test_trajectories_empty_when_no_actions() {
    let app = build_test_app();

    let response = app
        .oneshot(system_get("/observe/trajectories"))
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

#[tokio::test]
async fn test_intent_evidence_returns_richer_intent_candidates() {
    let state = test_state_with_turso().await;
    let intent = "Send an invoice to the customer";

    state
        .persist_trajectory_entry(&TrajectoryEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: "default".to_string(),
            entity_type: "Invoice".to_string(),
            entity_id: "invoice-1".to_string(),
            action: "GenerateInvoice".to_string(),
            success: false,
            from_status: None,
            to_status: None,
            error: Some("EntitySetNotFound: Invoice".to_string()),
            agent_id: Some("agent-1".to_string()),
            session_id: Some("session-1".to_string()),
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: None,
            spec_governed: None,
            agent_type: None,
            request_body: Some(serde_json::json!({"customer_id":"c-1"})),
            intent: Some(intent.to_string()),
            matched_policy_ids: None,
        })
        .await
        .unwrap();
    state
        .persist_trajectory_entry(&TrajectoryEntry {
            timestamp: sim_now().to_rfc3339(),
            tenant: "default".to_string(),
            entity_type: "InvoiceDraft".to_string(),
            entity_id: "draft-1".to_string(),
            action: "CreateDraft".to_string(),
            success: true,
            from_status: None,
            to_status: None,
            error: None,
            agent_id: Some("agent-1".to_string()),
            session_id: Some("session-1".to_string()),
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: None,
            spec_governed: None,
            agent_type: None,
            request_body: Some(serde_json::json!({"customer_id":"c-1"})),
            intent: Some(intent.to_string()),
            matched_policy_ids: None,
        })
        .await
        .unwrap();

    let app = build_app_with_state(state);
    let response = app
        .oneshot(system_get("/observe/evolution/intent-evidence"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let candidates = json["intent_candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 1);
    assert_eq!(
        candidates[0]["intent_title"],
        "Send An Invoice To The Customer"
    );
    assert_eq!(candidates[0]["suggested_kind"], "workaround");
    assert_eq!(json["workaround_patterns"][0]["occurrences"], 1);
}

async fn persist_feature_request_evidence(state: &ServerState, index: i64) {
    persist_feature_request_evidence_for_action(state, index, "GenerateReport").await;
}

async fn persist_feature_request_evidence_for_action(
    state: &ServerState,
    index: i64,
    action: &str,
) {
    let timestamp = (sim_now() + chrono::Duration::seconds(index)).to_rfc3339();
    persist_feature_request_evidence_for_action_at(state, index, action, &timestamp).await;
}

async fn persist_feature_request_evidence_for_action_at(
    state: &ServerState,
    index: i64,
    action: &str,
    timestamp: &str,
) {
    persist_feature_request_evidence_for_tenant_action_at(
        state, "default", index, action, timestamp,
    )
    .await;
}

async fn persist_feature_request_evidence_for_tenant_action_at(
    state: &ServerState,
    tenant: &str,
    index: i64,
    action: &str,
    timestamp: &str,
) {
    state
        .persist_trajectory_entry(&TrajectoryEntry {
            timestamp: timestamp.to_string(),
            tenant: tenant.to_string(),
            entity_type: "MissingCapability".to_string(),
            entity_id: format!("missing-{index}"),
            action: action.to_string(),
            success: false,
            from_status: None,
            to_status: None,
            error: Some("EntitySetNotFound: Report".to_string()),
            agent_id: Some("agent-1".to_string()),
            session_id: Some("session-1".to_string()),
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: Some(TrajectorySource::Platform),
            spec_governed: Some(true),
            agent_type: Some("swe".to_string()),
            request_body: None,
            intent: Some("Generate a report".to_string()),
            matched_policy_ids: None,
        })
        .await
        .expect("persist trajectory evidence");
}

#[test]
fn feature_request_identity_is_stable_across_replay_and_explicitly_versioned() {
    let entries = (0..3)
        .map(|index| TrajectoryEntry {
            timestamp: (sim_now() + chrono::Duration::seconds(index)).to_rfc3339(),
            tenant: "default".to_string(),
            entity_type: "MissingCapability".to_string(),
            entity_id: format!("missing-{index}"),
            action: "GenerateReport".to_string(),
            success: false,
            from_status: None,
            to_status: None,
            error: Some(format!("EntitySetNotFound: Report variant {index}")),
            agent_id: Some("agent-1".to_string()),
            session_id: Some("session-1".to_string()),
            authz_denied: None,
            denied_resource: None,
            denied_module: None,
            source: Some(TrajectorySource::Platform),
            spec_governed: Some(true),
            agent_type: Some("swe".to_string()),
            request_body: None,
            intent: Some("Generate a report".to_string()),
            matched_policy_ids: None,
        })
        .collect::<Vec<_>>();
    let generated = evolution::insight_generator::generate_feature_requests(&entries);
    let original = generated.first().expect("feature request at threshold");
    let mut reordered_entries = entries;
    reordered_entries.reverse();
    let reordered = evolution::insight_generator::generate_feature_requests(&reordered_entries);
    let replayed = reordered
        .first()
        .expect("reordered feature request at threshold");
    let tenant = TenantId::default();

    let original_id = evolution::stable_feature_request_id(&tenant, "v1", original);
    let replayed_id = evolution::stable_feature_request_id(&tenant, "v1", replayed);
    let revised_model_id = evolution::stable_feature_request_id(&tenant, "v2", original);

    assert_eq!(
        original_id, replayed_id,
        "evidence order must be normalized"
    );
    assert_eq!(
        original.description, replayed.description,
        "generated content must be independent of evidence arrival order",
    );
    assert_ne!(
        original_id, revised_model_id,
        "generator version changes must create explicit revisions",
    );
}

#[tokio::test]
async fn feature_request_get_is_a_pure_read() {
    let state = test_state_with_turso().await;
    for index in 0..3 {
        persist_feature_request_evidence(&state, index).await;
    }

    let store = state
        .platform_metadata_store()
        .expect("Turso metadata store");
    let app = build_app_with_state(state);

    let first = observe_json(app.clone(), "/observe/evolution/feature-requests").await;
    let second = observe_json(app.clone(), "/observe/evolution/feature-requests").await;
    let (concurrent_a, concurrent_b) = tokio::join!(
        observe_json(app.clone(), "/observe/evolution/feature-requests"),
        observe_json(app, "/observe/evolution/feature-requests"),
    );

    assert_eq!(
        first["total"], 0,
        "GET must not materialize feature requests"
    );
    assert_eq!(
        second["total"], 0,
        "repeated GET must remain side-effect free"
    );
    assert_eq!(concurrent_a["total"], 0);
    assert_eq!(concurrent_b["total"], 0);
    assert!(
        store
            .list_feature_requests(None)
            .await
            .expect("list persisted feature requests")
            .is_empty(),
        "GET must not write the feature-request projection",
    );
}

#[tokio::test]
async fn sentinel_materializes_feature_requests_idempotently_by_evidence_revision() {
    let state = test_state_with_feature_request_runtime().await;
    for index in 0..3 {
        persist_feature_request_evidence(&state, index).await;
    }
    let store = state
        .platform_metadata_store()
        .expect("Turso metadata store");
    let app = build_app_with_state(state.clone());
    let generated = evolution::insight_generator::generate_feature_requests(
        &state.load_trajectory_entries(100).await,
    );
    let legacy = generated.first().expect("feature request at threshold");
    let legacy_id = "FR-2026-0123456789ab";
    store
        .upsert_feature_request(
            legacy_id,
            &format!("{:?}", legacy.category),
            &legacy.description,
            legacy.frequency as i64,
            &serde_json::to_string(&legacy.trajectory_refs).expect("serialize legacy refs"),
            "Open",
            None,
        )
        .await
        .expect("seed legacy GET-created projection");
    store
        .update_feature_request(legacy_id, "Planned", Some("Reviewed before upgrade"))
        .await
        .expect("seed human review on legacy projection");

    let (first, concurrent_retry) = tokio::join!(
        app.clone()
            .oneshot(system_post("/api/evolution/sentinel/check", "")),
        app.clone()
            .oneshot(system_post("/api/evolution/sentinel/check", "")),
    );
    let first = first.expect("first sentinel request");
    let concurrent_retry = concurrent_retry.expect("concurrent sentinel retry");
    assert_eq!(first.status(), StatusCode::OK);
    assert_eq!(concurrent_retry.status(), StatusCode::OK);
    let first_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(first.into_body(), 1024 * 1024)
            .await
            .expect("first sentinel body"),
    )
    .expect("first sentinel JSON");
    let stable_id = first_json["feature_request_ids"][0]
        .as_str()
        .expect("stable feature-request id")
        .to_string();
    let concurrent_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(concurrent_retry.into_body(), 1024 * 1024)
            .await
            .expect("concurrent sentinel body"),
    )
    .expect("concurrent sentinel JSON");
    assert_eq!(concurrent_json["feature_request_ids"][0], stable_id);
    let first_entity = state
        .get_tenant_entity_state(
            &TenantId::new("temper-system"),
            "FeatureRequest",
            &stable_id,
        )
        .await
        .expect("first FeatureRequest entity");
    let first_event_count = first_entity.state.events.len();
    let creation_event_count = first_entity
        .state
        .events
        .iter()
        .filter(|event| event.action == "CreateFeatureRequest")
        .count();
    assert_eq!(
        creation_event_count, 1,
        "concurrent materialization must append exactly one creation event",
    );
    let migrated_rows = store
        .list_feature_requests(None)
        .await
        .expect("list reconciled legacy projection");
    assert_eq!(
        migrated_rows.len(),
        1,
        "upgrade reconciliation must replace legacy duplicates with one canonical row",
    );
    assert_eq!(migrated_rows[0].id, stable_id);
    assert_eq!(migrated_rows[0].disposition, "Planned");
    assert_eq!(
        migrated_rows[0].developer_notes.as_deref(),
        Some("Reviewed before upgrade"),
        "upgrade reconciliation must preserve human review state",
    );
    store
        .update_feature_request(&stable_id, "Planned", Some("Reviewed after migration"))
        .await
        .expect("record human feature-request review");

    let second = app
        .clone()
        .oneshot(system_post("/api/evolution/sentinel/check", ""))
        .await
        .expect("second sentinel request");
    assert_eq!(second.status(), StatusCode::OK);
    let second_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(second.into_body(), 1024 * 1024)
            .await
            .expect("second sentinel body"),
    )
    .expect("second sentinel JSON");
    assert_eq!(second_json["feature_request_ids"][0], stable_id);
    let stable_rows = store
        .list_feature_requests(None)
        .await
        .expect("list stable projection");
    assert_eq!(stable_rows.len(), 1, "repeat must keep one projection row");
    assert_eq!(stable_rows[0].disposition, "Planned");
    assert_eq!(
        stable_rows[0].developer_notes.as_deref(),
        Some("Reviewed after migration"),
        "materialization must preserve mutable human review state",
    );
    let stable_entity = state
        .get_tenant_entity_state(
            &TenantId::new("temper-system"),
            "FeatureRequest",
            &stable_id,
        )
        .await
        .expect("stable FeatureRequest entity");
    assert_eq!(
        stable_entity.state.events.len(),
        first_event_count,
        "durable idempotency must suppress duplicate CreateFeatureRequest events",
    );

    persist_feature_request_evidence(&state, 3).await;
    let revised = app
        .oneshot(system_post("/api/evolution/sentinel/check", ""))
        .await
        .expect("revised sentinel request");
    assert_eq!(revised.status(), StatusCode::OK);
    let revised_json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(revised.into_body(), 1024 * 1024)
            .await
            .expect("revised sentinel body"),
    )
    .expect("revised sentinel JSON");
    assert_ne!(revised_json["feature_request_ids"][0], stable_id);
    assert_eq!(
        store
            .list_feature_requests(None)
            .await
            .expect("list revised projection")
            .len(),
        2,
        "changed evidence must create an explicit new revision",
    );
}

#[tokio::test]
async fn sentinel_reconciles_only_matching_legacy_evidence_and_deduplicates_notes() {
    let state = test_state_with_feature_request_runtime().await;
    for index in 0..3 {
        let timestamp = (sim_now() + chrono::Duration::seconds(index)).to_rfc3339();
        persist_feature_request_evidence_for_action_at(&state, index, "GenerateReport", &timestamp)
            .await;
        persist_feature_request_evidence_for_action_at(
            &state,
            index,
            "ExportDashboard",
            &timestamp,
        )
        .await;
    }
    let generated = evolution::insight_generator::generate_feature_requests(
        &state.load_trajectory_entries(100).await,
    );
    assert_eq!(generated.len(), 2);
    let report = generated
        .iter()
        .find(|record| record.description.contains("'GenerateReport'"))
        .expect("report feature request");
    let dashboard = generated
        .iter()
        .find(|record| record.description.contains("'ExportDashboard'"))
        .expect("dashboard feature request");
    assert_eq!(
        report.trajectory_refs, dashboard.trajectory_refs,
        "regression requires both actions to share the same evidence timestamps",
    );

    let store = state
        .platform_metadata_store()
        .expect("Turso metadata store");
    let report_id = evolution::stable_feature_request_id(&TenantId::default(), "v1", report);
    let seeds = [
        (
            report_id.as_str(),
            report,
            "Planned",
            Some("  Report note B  \n\nReport note A"),
        ),
        (
            "FR-2026-222222222222",
            report,
            "Planned",
            Some("Report note B"),
        ),
        (
            "FR-2026-333333333333",
            dashboard,
            "Acknowledged",
            Some("Dashboard note"),
        ),
    ];
    for (id, record, disposition, notes) in seeds {
        store
            .upsert_feature_request(
                id,
                &format!("{:?}", record.category),
                &record.description,
                record.frequency as i64,
                &serde_json::to_string(&record.trajectory_refs).expect("serialize legacy refs"),
                disposition,
                notes,
            )
            .await
            .expect("seed feature-request projection");
    }

    let response = build_app_with_state(state)
        .oneshot(system_post("/api/evolution/sentinel/check", ""))
        .await
        .expect("sentinel reconciliation request");
    assert_eq!(response.status(), StatusCode::OK);

    let rows = store
        .list_feature_requests(None)
        .await
        .expect("list reconciled feature requests");
    assert_eq!(rows.len(), 2, "each action must retain one canonical row");
    assert!(
        rows.iter().all(|row| !row.id.starts_with("FR-2026-")),
        "all legacy projections must be removed",
    );
    let report_row = rows
        .iter()
        .find(|row| row.description.contains("'GenerateReport'"))
        .expect("canonical report row");
    assert_eq!(report_row.disposition, "Planned");
    assert_eq!(
        report_row.developer_notes.as_deref(),
        Some("  Report note B  \n\nReport note A"),
        "retrying after one legacy row was deleted must preserve canonical note bytes",
    );
    let dashboard_row = rows
        .iter()
        .find(|row| row.description.contains("'ExportDashboard'"))
        .expect("canonical dashboard row");
    assert_eq!(dashboard_row.disposition, "Acknowledged");
    assert_eq!(
        dashboard_row.developer_notes.as_deref(),
        Some("Dashboard note"),
        "same-timestamp evidence from another action must not contaminate review state",
    );
}

#[tokio::test]
async fn sentinel_does_not_claim_tenant_ambiguous_legacy_feature_requests() {
    let state = test_state_with_feature_request_runtime().await;
    for index in 0..3 {
        let timestamp = (sim_now() + chrono::Duration::seconds(index)).to_rfc3339();
        for tenant in ["tenant-a", "tenant-b"] {
            persist_feature_request_evidence_for_tenant_action_at(
                &state,
                tenant,
                index,
                "GenerateReport",
                &timestamp,
            )
            .await;
        }
    }
    let entries = state.load_trajectory_entries(100).await;
    let tenant_a_entries = entries
        .iter()
        .filter(|entry| entry.tenant == "tenant-a")
        .cloned()
        .collect::<Vec<_>>();
    let generated = evolution::insight_generator::generate_feature_requests(&tenant_a_entries);
    let legacy = generated.first().expect("tenant feature request");
    let store = state
        .platform_metadata_store()
        .expect("Turso metadata store");
    for (legacy_id, notes) in [
        ("FR-2026-aaaaaaaaaaaa", "Tenant A review"),
        ("FR-2026-bbbbbbbbbbbb", "Tenant B review"),
    ] {
        store
            .upsert_feature_request(
                legacy_id,
                &format!("{:?}", legacy.category),
                &legacy.description,
                legacy.frequency as i64,
                &serde_json::to_string(&legacy.trajectory_refs).expect("serialize legacy refs"),
                "Planned",
                Some(notes),
            )
            .await
            .expect("seed tenant-ambiguous legacy projection");
    }

    for tenant in ["tenant-a", "tenant-b"] {
        let ids = evolution::materialize_feature_requests_for_test(
            &state,
            &TenantId::new(tenant),
            &entries,
        )
        .await
        .expect("tenant feature-request materialization");
        assert_eq!(ids.len(), 1);
    }

    let rows = store
        .list_feature_requests(None)
        .await
        .expect("list tenant-isolated feature requests");
    assert_eq!(
        rows.len(),
        4,
        "each tenant gets a canonical row while both ambiguous legacy rows remain",
    );
    for (legacy_id, notes) in [
        ("FR-2026-aaaaaaaaaaaa", "Tenant A review"),
        ("FR-2026-bbbbbbbbbbbb", "Tenant B review"),
    ] {
        let row = rows
            .iter()
            .find(|row| row.id == legacy_id)
            .expect("ambiguous legacy row must remain");
        assert_eq!(row.disposition, "Planned");
        assert_eq!(row.developer_notes.as_deref(), Some(notes));
    }
    let canonical_rows = rows
        .iter()
        .filter(|row| !row.id.starts_with("FR-2026-"))
        .collect::<Vec<_>>();
    assert_eq!(canonical_rows.len(), 2);
    assert!(
        canonical_rows
            .iter()
            .all(|row| row.disposition == "Open" && row.developer_notes.is_none()),
        "neither tenant may consume review state from an ambiguous legacy row",
    );
}

#[tokio::test]
async fn sentinel_retry_after_restart_reuses_the_durable_feature_request() {
    let db_url = feature_request_db_url();
    let _ = std::fs::remove_file(db_url.strip_prefix("file:").unwrap_or(&db_url));

    let (stable_id, event_count) = {
        let state = test_state_with_feature_request_runtime_at(&db_url).await;
        for index in 0..3 {
            persist_feature_request_evidence(&state, index).await;
        }
        let response = build_app_with_state(state.clone())
            .oneshot(system_post("/api/evolution/sentinel/check", ""))
            .await
            .expect("sentinel request before restart");
        assert_eq!(response.status(), StatusCode::OK);
        let json: serde_json::Value = serde_json::from_slice(
            &axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("sentinel body before restart"),
        )
        .expect("sentinel JSON before restart");
        let stable_id = json["feature_request_ids"][0]
            .as_str()
            .expect("stable feature-request id")
            .to_string();
        let entity = state
            .get_tenant_entity_state(
                &TenantId::new("temper-system"),
                "FeatureRequest",
                &stable_id,
            )
            .await
            .expect("FeatureRequest before restart");
        (stable_id, entity.state.events.len())
    };

    let restarted_state = test_state_with_feature_request_runtime_at(&db_url).await;
    let response = build_app_with_state(restarted_state.clone())
        .oneshot(system_post("/api/evolution/sentinel/check", ""))
        .await
        .expect("sentinel retry after restart");
    assert_eq!(response.status(), StatusCode::OK);
    let json: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("sentinel body after restart"),
    )
    .expect("sentinel JSON after restart");
    assert_eq!(json["feature_request_ids"][0], stable_id);
    let rehydrated = restarted_state
        .get_tenant_entity_state(
            &TenantId::new("temper-system"),
            "FeatureRequest",
            &stable_id,
        )
        .await
        .expect("rehydrated FeatureRequest");
    assert_eq!(
        rehydrated.state.events.len(),
        event_count,
        "restart retry must not append a duplicate creation event",
    );
    assert_eq!(
        restarted_state
            .platform_metadata_store()
            .expect("Turso metadata store")
            .list_feature_requests(None)
            .await
            .expect("list projections after restart")
            .len(),
        1,
        "restart retry must retain one projection row",
    );
}

// -- Sentinel endpoint tests --

#[tokio::test]
async fn test_sentinel_check_no_alerts_on_clean_state() {
    let app = build_test_app();

    let response = app
        .oneshot(system_post("/api/evolution/sentinel/check", ""))
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
        .oneshot(system_post("/api/evolution/sentinel/check", ""))
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
        .oneshot(system_get("/observe/evolution/records"))
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
        .oneshot(system_post("/api/evolution/sentinel/check", ""))
        .await
        .unwrap();

    // Now check evolution records.
    let response = app
        .oneshot(system_get("/observe/evolution/records"))
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
        .oneshot(system_get("/observe/evolution/records/O-2024-nonexistent"))
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
        .platform_turso_store()
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
                    .header("X-Temper-Principal-Kind", "admin")
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
                .header("X-Temper-Principal-Kind", "admin")
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
    state.set_storage_stack(StorageStack::from_turso(turso));

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
    let turso = state.platform_turso_store().expect("turso configured");
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
        .oneshot(system_get("/observe/evolution/insights"))
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
