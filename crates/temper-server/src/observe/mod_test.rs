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
use crate::state::TrajectoryEntry;
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

fn build_test_app() -> Router {
    let state = test_state_with_registry();
    Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state)
}

fn admin_security_context() -> temper_authz::SecurityContext {
    temper_authz::SecurityContext {
        principal: temper_authz::Principal {
            id: "admin-1".to_string(),
            kind: temper_authz::PrincipalKind::Admin,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "observe-test-admin".to_string(),
    }
}

fn customer_security_context(id: &str) -> temper_authz::SecurityContext {
    temper_authz::SecurityContext {
        principal: temper_authz::Principal {
            id: id.to_string(),
            kind: temper_authz::PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "observe-test-customer".to_string(),
    }
}

fn with_security_context(
    mut request: Request<Body>,
    security_context: temper_authz::SecurityContext,
) -> Request<Body> {
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::default(),
            security_context,
        ));
    request
}

fn with_tenant_security_context(
    mut request: Request<Body>,
    tenant: &str,
    security_context: temper_authz::SecurityContext,
) -> Request<Body> {
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            security_context,
        ));
    request
}

fn admin_request(request: Request<Body>) -> Request<Body> {
    with_security_context(request, admin_security_context())
}

fn system_request(request: Request<Body>) -> Request<Body> {
    with_security_context(request, temper_authz::SecurityContext::system())
}

fn agent_request(request: Request<Body>) -> Request<Body> {
    with_security_context(
        request,
        temper_authz::SecurityContext::from_resolved_identity("agent-1", "swe", Some("session-1")),
    )
}

fn customer_request(request: Request<Body>, id: &str) -> Request<Body> {
    with_security_context(request, customer_security_context(id))
}

#[tokio::test]
async fn typed_admin_kind_does_not_bypass_observe_cedar() {
    let response = build_app_with_state(test_state_with_registry())
        .oneshot(admin_request(
            Request::get("/observe/specs")
                .body(Body::empty())
                .expect("request should build"),
        ))
        .await
        .expect("request should run");
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
}

/// Build a GET request with kernel System authority for functional tests.
fn system_get(uri: &str) -> Request<Body> {
    system_request(Request::get(uri).body(Body::empty()).unwrap())
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

/// Build a POST request with kernel System authority for functional tests.
fn system_post(uri: &str, body: &str) -> Request<Body> {
    system_request(
        Request::post(uri)
            .header("Content-Type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
}

fn agent_post(uri: &str, body: impl Into<Body>) -> Request<Body> {
    agent_request(
        Request::post(uri)
            .header("X-Tenant-Id", "default")
            .body(body.into())
            .unwrap(),
    )
}

fn agent_get(uri: &str) -> Request<Body> {
    agent_request(
        Request::get(uri)
            .header("X-Tenant-Id", "default")
            .body(Body::empty())
            .unwrap(),
    )
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
        .reload_tenant_policies("default", ADMIN_MANAGE_POLICIES_POLICY)
        .expect("admin policy should parse");
}

fn install_admin_submit_specs_policy(state: &ServerState, tenant: &str) {
    state
        .authz
        .reload_tenant_policies(tenant, ADMIN_SUBMIT_SPECS_POLICY)
        .expect("submit_specs policy should parse");
}

fn install_admin_file_read_policy(state: &ServerState) {
    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"
permit(
  principal == Admin::"admin-1",
  action == Action::"read",
  resource is File
);
permit(
  principal == Admin::"admin-1",
  action == Action::"read",
  resource is FileVersion
);
"#,
        )
        .expect("admin file read policy should parse");
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
        .get_pending_decision("default", decision_id)
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
        .oneshot(agent_request(
            Request::delete("/api/wasm/modules/git_receive_pack")
                .header("X-Tenant-Id", "default")
                .body(Body::empty())
                .unwrap(),
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
        .get_pending_decision("default", decision_id)
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
        .oneshot(system_post(
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
        .oneshot(admin_request(
            Request::post(format!(
                "/api/tenants/default/decisions/{decision_id}/approve"
            ))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"scope":{"principal":"this_agent","action":"this_action","resource":"this_resource","duration":"always"},"decided_by":"admin-1"}"#))
            .unwrap(),
        ))
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
        .oneshot(admin_request(
            Request::get(format!("/api/tenants/default/decisions/{decision_id}"))
                .body(Body::empty())
                .unwrap(),
        ))
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
    install_admin_file_read_policy(&state);
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
    install_admin_file_read_policy(&state);
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
    install_admin_file_read_policy(&state);
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

    // The params/hint contract (consumed by generated typed clients):
    // AddItem declares two bare-named params (default "string" type, spec
    // order preserved) and a hint; RemoveItem declares no hint (empty, not
    // null).
    let add_item = detail
        .actions
        .iter()
        .find(|a| a.name == "AddItem")
        .expect("Order fixture declares AddItem");
    let param_pairs: Vec<(&str, &str)> = add_item
        .params
        .iter()
        .map(|p| (p.name.as_str(), p.param_type.as_str()))
        .collect();
    assert_eq!(
        param_pairs,
        vec![("ProductId", "string"), ("Quantity", "string")]
    );
    assert!(add_item.hint.starts_with("Add a product"));
    let remove_item = detail
        .actions
        .iter()
        .find(|a| a.name == "RemoveItem")
        .expect("Order fixture declares RemoveItem");
    assert_eq!(remove_item.params.len(), 1);
    assert_eq!(remove_item.params[0].name, "ItemId");
    assert_eq!(remove_item.hint, "");

    // Wire-level contract: the type field serializes under the key "type",
    // and params are objects, not bare strings.
    let raw: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let raw_add_item = raw["actions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "AddItem")
        .unwrap();
    assert_eq!(raw_add_item["params"][0]["name"], "ProductId");
    assert_eq!(raw_add_item["params"][0]["type"], "string");
    assert_eq!(raw_add_item["hint"], add_item.hint);
}

#[test]
fn action_param_detail_serializes_typed_params_under_type_key() {
    // Typed params (e.g. `{ name = "sleep_seconds", type = "uint64" }` in a
    // spec) reach ActionParamDetail through ActionParam::name()/param_type();
    // this locks the wire shape those values serialize into.
    let detail = ActionParamDetail {
        name: "sleep_seconds".to_string(),
        param_type: "uint64".to_string(),
    };
    let json = serde_json::to_value(&detail).unwrap();
    assert_eq!(
        json,
        serde_json::json!({"name": "sleep_seconds", "type": "uint64"})
    );
    let back: ActionParamDetail = serde_json::from_value(json).unwrap();
    assert_eq!(back.name, "sleep_seconds");
    assert_eq!(back.param_type, "uint64");
}

#[tokio::test]
async fn spec_detail_reports_the_registered_content_hash() {
    // The reason the field exists: a producer that recomputes the digest from
    // a spec file gets a different one the moment any deploy path rewrites
    // that file, and every conformance check for the run is then refused. This
    // is the digest the kernel actually holds.
    let app = build_test_app();
    let response = app
        .oneshot(system_get("/observe/specs/Order"))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let raw: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        raw["spec_version"],
        serde_json::json!(temper_store_turso::spec_content_hash(ORDER_IOA)),
        "the reported digest must be the one a conformance check compares against"
    );

    let detail: SpecDetail = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        detail.spec_version.len(),
        64,
        "a sha256 digest is 64 hex characters: {}",
        detail.spec_version
    );
    assert!(
        detail
            .spec_version
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
        "the digest is reported bare and lowercase, not algorithm-qualified: {}",
        detail.spec_version
    );
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
    install_admin_submit_specs_policy(&state, "nested-inline");
    let app = build_app_with_state(state.clone());

    let response = app
        .oneshot(with_tenant_security_context(
            Request::post("/api/specs/load-inline")
                .header("Content-Type", "application/json")
                .body(Body::from(
                    serde_json::json!({
                        "tenant": "nested-inline",
                        "specs": {
                            "InlineProbe/model.csdl.xml": CSDL_XML,
                            "InlineProbe/order.ioa.toml": ORDER_IOA
                        }
                    })
                    .to_string(),
                ))
                .unwrap(),
            "nested-inline",
            admin_security_context(),
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
async fn test_load_inline_cannot_bundle_policy_authority() {
    let state = test_state_with_registry();
    let app = build_app_with_state(state.clone());
    let response = app
        .oneshot(system_post(
            "/api/specs/load-inline",
            &serde_json::json!({
                "tenant": "default",
                "specs": {
                    "model.csdl.xml": CSDL_XML,
                    "order.ioa.toml": ORDER_IOA
                },
                "cedar_policies": "permit(principal, action, resource);"
            })
            .to_string(),
        ))
        .await
        .expect("request should run");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert!(
        state
            .authorize_with_context(
                &admin_security_context(),
                "delete",
                "Secret",
                &BTreeMap::new(),
                "default",
            )
            .is_err(),
        "bundled policy text must never become active"
    );
}

#[tokio::test]
async fn tenant_decisions_require_auth_without_leaking_rows() {
    let state = test_state_with_turso().await;
    state
        .platform_turso_store()
        .expect("turso configured")
        .upsert_pending_decision(
            "PD-private",
            "default",
            "pending",
            r#"{"id":"PD-private","tenant":"default","status":"pending","marker":"tenant-secret"}"#,
        )
        .await
        .expect("seed private decision");
    let app = build_app_with_state(state);

    let response = app
        .oneshot(
            Request::get("/api/tenants/default/decisions")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    assert!(!String::from_utf8_lossy(&body).contains("tenant-secret"));
}

#[tokio::test]
async fn tenant_decision_stream_requires_auth() {
    let state = test_state_with_registry();
    let app = build_app_with_state(state);

    let response = app
        .oneshot(
            Request::get("/api/tenants/default/decisions/stream")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let ct = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(!ct.contains("text/event-stream"));
}

#[tokio::test]
async fn test_tenant_decision_mutations_require_manage_policies() {
    let state = test_state_with_registry();
    install_admin_policy(&state);
    let app = build_app_with_state(state);

    let deny_approve = app
        .clone()
        .oneshot(customer_request(
            Request::post("/api/tenants/default/decisions/PD-does-not-exist/approve")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"scope":{"principal":"this_agent","action":"this_action","resource":"this_resource","duration":"always"}}"#))
                .unwrap(),
            "cust-1",
        ))
        .await
        .unwrap();
    assert_eq!(deny_approve.status(), StatusCode::FORBIDDEN);

    let deny_deny = app
        .oneshot(customer_request(
            Request::post("/api/tenants/default/decisions/PD-does-not-exist/deny")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"decided_by":"cust-1"}"#))
                .unwrap(),
            "cust-1",
        ))
        .await
        .unwrap();
    assert_eq!(deny_deny.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn denied_principal_cannot_approve_or_deny_even_with_manage_policies() {
    let state = test_state_with_turso().await;
    state
        .authz
        .reload_tenant_policies(
            "default",
            r#"
permit(
  principal is Agent,
  action == Action::"manage_policies",
  resource is PolicySet
);
permit(
  principal is Admin,
  action == Action::"manage_policies",
  resource is PolicySet
);
"#,
        )
        .expect("manage_policies policy should parse");

    let pending = crate::state::PendingDecision::from_denial(
        "default",
        "agent-1",
        "Assign",
        "Issue",
        "issue-1",
        serde_json::json!({"id":"issue-1"}),
        "test denial",
        None,
    );
    let decision_id = pending.id.clone();
    state
        .persist_pending_decision(&pending)
        .await
        .expect("persist pending decision");

    let app = build_app_with_state(state.clone());
    let approve_body = r#"{"scope":{"principal":"this_agent","action":"this_action","resource":"this_resource","duration":"always"}}"#;

    let self_approve = app
        .clone()
        .oneshot(agent_request(
            Request::post(format!(
                "/api/tenants/default/decisions/{decision_id}/approve"
            ))
            .header("content-type", "application/json")
            .body(Body::from(approve_body))
            .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(
        self_approve.status(),
        StatusCode::FORBIDDEN,
        "denied agent must not approve their own decision"
    );

    let self_deny = app
        .clone()
        .oneshot(agent_request(
            Request::post(format!("/api/tenants/default/decisions/{decision_id}/deny"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(
        self_deny.status(),
        StatusCode::FORBIDDEN,
        "denied agent must not deny their own decision"
    );

    let operator_approve = app
        .oneshot(admin_request(
            Request::post(format!(
                "/api/tenants/default/decisions/{decision_id}/approve"
            ))
            .header("content-type", "application/json")
            .body(Body::from(approve_body))
            .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(
        operator_approve.status(),
        StatusCode::OK,
        "a different principal with manage_policies must still approve"
    );
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
        .oneshot(admin_request(
            Request::post(format!(
                "/api/tenants/default/decisions/{decision_id}/approve"
            ))
            .header("content-type", "application/json")
            .body(Body::from(r#"{"scope":{"principal":"this_agent","action":"this_action","resource":"this_resource","duration":"always"},"decided_by":"admin-1"}"#))
            .unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Verify decision status unchanged in Turso.
    let turso = state.platform_turso_store().expect("turso configured");
    let data_str = turso
        .get_pending_decision("default", &decision_id)
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
            capture_seq: None,
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
            capture_seq: None,
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
        .insert_evolution_record(temper_store_turso::TursoEvolutionRecordInsert {
            tenant: "default",
            id: &obs.header.id,
            record_type: "Observation",
            status: &format!("{:?}", obs.header.status),
            created_by: &obs.header.created_by,
            derived_from: obs.header.derived_from.as_deref(),
            data_json: &data_json,
        })
        .await
        .expect("insert O-Record to Turso");

    let app = Router::new()
        .nest("/observe", build_observe_router())
        .nest("/api", crate::api::build_api_router())
        .with_state(state);

    // Create a D-Record decision.
    let response = app
        .clone()
        .oneshot(system_post(
            "/api/evolution/records/O-test-decide/decide",
            r#"{"decision":"approved","decided_by":"alice@example.com","rationale":"Looks good"}"#,
        ))
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
        .oneshot(system_post(
            "/api/evolution/records/O-nonexistent/decide",
            r#"{"decision":"rejected","decided_by":"bob","rationale":"nope"}"#,
        ))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::NOT_FOUND);
}

// -- Workflow endpoint tests --

#[tokio::test]
async fn test_workflows_returns_tenant_data() {
    let app = build_test_app();
    let response = app.oneshot(system_get("/observe/workflows")).await.unwrap();

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

#[path = "mod_test/load_dir_tests.rs"]
mod load_dir_tests;
