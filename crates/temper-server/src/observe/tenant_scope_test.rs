//! Observe routes answer from the caller's own tenant, and per-entity reads
//! make the same Cedar decision as the OData read of that entity.

use super::*;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

use crate::registry::{SpecRegistry, VerificationStatus};
use crate::request_context::AgentContext;
use crate::state::WasmInvocationEntry;
use crate::storage::StorageStack;

const CSDL_XML: &str = include_str!("../../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

/// Tenants `alpha` and `zeta` on one server; only the listed tenants define `Order`.
fn state_with_tenants(with_order: &[&str], without_order: &[&str]) -> ServerState {
    let mut registry = SpecRegistry::new();
    for tenant in with_order {
        let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
        registry.register_tenant(*tenant, csdl, CSDL_XML.to_string(), &[("Order", ORDER_IOA)]);
    }
    for tenant in without_order {
        let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
        registry.register_tenant(*tenant, csdl, CSDL_XML.to_string(), &[]);
    }
    ServerState::from_registry(ActorSystem::new("observe-tenant-scope"), registry)
}

fn app(state: ServerState) -> Router {
    Router::new()
        .nest("/observe", build_observe_router())
        .with_state(state)
}

fn as_tenant(
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

fn system_in(tenant: &str, method: &str, uri: &str) -> Request<Body> {
    as_tenant(
        Request::builder()
            .method(method)
            .uri(uri)
            .body(Body::empty())
            .expect("request should build"),
        tenant,
        temper_authz::SecurityContext::system(),
    )
}

fn agent_in(tenant: &str, uri: &str) -> Request<Body> {
    as_tenant(
        Request::get(uri)
            .body(Body::empty())
            .expect("request should build"),
        tenant,
        temper_authz::SecurityContext::from_resolved_identity("agent-1", "swe", Some("session-1")),
    )
}

async fn json_of(response: axum::response::Response) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("body should read");
    serde_json::from_slice(&body).expect("body should be JSON")
}

#[tokio::test]
async fn verification_routes_do_not_resolve_a_type_from_another_tenant() {
    let state = state_with_tenants(&["alpha"], &["zeta"]);
    let alpha = TenantId::new("alpha");
    state.registry.write().unwrap().set_verification_status(
        &alpha,
        "Order",
        VerificationStatus::Pending,
    );

    for (method, uri) in [
        ("GET", "/observe/paths/Order"),
        ("GET", "/observe/simulation/Order?seed=1&ticks=10"),
        ("POST", "/observe/verify/Order"),
    ] {
        let response = app(state.clone())
            .oneshot(system_in("zeta", method, uri))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{method} {uri} must not find alpha's Order for a zeta caller"
        );
    }

    assert!(
        matches!(
            state
                .registry
                .read()
                .unwrap()
                .get_verification_status(&alpha, "Order"),
            Some(VerificationStatus::Pending)
        ),
        "alpha's verification status is left as alpha set it"
    );
}

#[tokio::test]
async fn verification_routes_resolve_the_callers_own_type() {
    let state = state_with_tenants(&["alpha", "zeta"], &[]);
    let response = app(state)
        .oneshot(system_in("zeta", "GET", "/observe/paths/Order"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
}

fn invocation(
    tenant: &str,
    entity_id: &str,
    module: &str,
    success: bool,
    at: &str,
) -> WasmInvocationEntry {
    WasmInvocationEntry {
        timestamp: at.to_string(),
        tenant: tenant.to_string(),
        entity_type: "Order".to_string(),
        entity_id: entity_id.to_string(),
        module_name: module.to_string(),
        trigger_action: "SubmitOrder".to_string(),
        callback_action: None,
        success,
        error: (!success).then(|| format!("{tenant} error")),
        duration_ms: 5,
        authz_denied: None,
    }
}

/// One Turso database shared by every tenant, the stack serve builds without
/// tenant routing.
async fn state_with_shared_store() -> ServerState {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let path = format!("/tmp/temper-observe-scope-{}-{id}.db", std::process::id());
    let _ = std::fs::remove_file(&path);
    let turso = TursoEventStore::new(&format!("file:{path}"), None)
        .await
        .expect("create local turso db");
    let mut state = state_with_tenants(&["tenant-a", "tenant-b"], &[]);
    state.set_storage_stack(StorageStack::from_turso(turso));
    state
}

#[tokio::test]
async fn wasm_invocation_history_lists_only_the_callers_tenant() {
    let state = state_with_shared_store().await;
    for entry in [
        invocation(
            "tenant-a",
            "a-1",
            "shared_name",
            true,
            "2026-01-01T00:00:01Z",
        ),
        invocation(
            "tenant-b",
            "b-1",
            "shared_name",
            false,
            "2026-01-01T00:00:02Z",
        ),
        invocation("tenant-b", "b-2", "b_only", true, "2026-01-01T00:00:03Z"),
    ] {
        state
            .persist_wasm_invocation(&entry)
            .await
            .expect("row persists");
    }

    let response = app(state.clone())
        .oneshot(system_in(
            "tenant-a",
            "GET",
            "/observe/wasm/invocations?limit=1",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_of(response).await;
    let rows = json["invocations"].as_array().expect("invocations array");
    assert_eq!(rows.len(), 1, "the limit counts the caller's own rows");
    assert_eq!(rows[0]["tenant"], "tenant-a");
    assert_eq!(rows[0]["entity_id"], "a-1");

    state.register_wasm_module(&TenantId::new("tenant-a"), "shared_name", "hash-a");
    let response = app(state)
        .oneshot(system_in("tenant-a", "GET", "/observe/wasm/modules"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let json = json_of(response).await;
    let module = json["modules"]
        .as_array()
        .expect("modules array")
        .iter()
        .find(|m| m["module_name"] == "shared_name")
        .expect("tenant-a's module is listed")
        .clone();
    assert_eq!(module["total_invocations"], 1);
    assert_eq!(module["success_count"], 1);
}

const ENTITY_READ_POLICY: &str = r#"
permit(principal, action == Action::"read", resource is Order);
forbid(principal, action == Action::"read", resource == Order::"order-b");
forbid(principal, action == Action::"read", resource is Order) when { resource has Status && resource.Status == "Submitted" };
"#;

async fn state_with_entity_read_policy() -> ServerState {
    let state = state_with_tenants(&["default"], &[]);
    let tenant = TenantId::default();
    for (id, actions) in [
        ("order-a", &["AddItem"][..]),
        ("order-b", &["AddItem"][..]),
        ("order-c", &["AddItem", "SubmitOrder"][..]),
    ] {
        for action in actions {
            let result = state
                .dispatch_tenant_action(
                    &tenant,
                    "Order",
                    id,
                    action,
                    serde_json::json!({"ProductId": "p1"}),
                    &AgentContext::default(),
                )
                .await;
            assert!(result.is_ok_and(|r| r.success), "{id} {action} failed");
        }
    }
    state
        .authz
        .reload_tenant_policies("default", ENTITY_READ_POLICY)
        .expect("policy should parse");
    state
}

#[tokio::test]
async fn entity_history_wait_and_events_follow_the_entity_read_decision() {
    let state = state_with_entity_read_policy().await;
    let routes = [
        "history",
        "wait?statuses=Draft,Submitted&timeout_ms=50&poll_ms=10",
        "events",
    ];
    for (id, expected) in [
        ("order-a", StatusCode::OK),
        ("order-b", StatusCode::FORBIDDEN),
        ("order-c", StatusCode::FORBIDDEN),
    ] {
        for route in routes {
            let uri = format!("/observe/entities/Order/{id}/{route}");
            let response = app(state.clone())
                .oneshot(agent_in("default", &uri))
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{uri}");
        }
    }
}

#[tokio::test]
async fn entity_wait_history_and_events_of_a_missing_entity_are_not_found() {
    let state = state_with_entity_read_policy().await;
    for route in [
        "wait?statuses=Draft&timeout_ms=50&poll_ms=10",
        "history",
        "events",
    ] {
        let uri = format!("/observe/entities/Order/no-such-order/{route}");
        let response = app(state.clone())
            .oneshot(agent_in("default", &uri))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
    }
}

async fn submit(state: &ServerState, id: &str) {
    let result = state
        .dispatch_tenant_action(
            &TenantId::default(),
            "Order",
            id,
            "SubmitOrder",
            serde_json::json!({}),
            &AgentContext::default(),
        )
        .await;
    assert!(result.is_ok_and(|r| r.success), "{id} SubmitOrder failed");
}

#[tokio::test]
async fn entity_wait_answers_not_found_when_the_entity_is_deleted_while_waiting() {
    let state = state_with_entity_read_policy().await;
    let uri = "/observe/entities/Order/order-a/wait?statuses=Shipped&timeout_ms=3000&poll_ms=10";
    let wait = tokio::spawn(app(state.clone()).oneshot(agent_in("default", uri)));
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    state
        .delete_tenant_entity(&TenantId::default(), "Order", "order-a")
        .await
        .expect("delete succeeds");

    let response = wait.await.expect("wait task").unwrap();
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(
        !state.entity_exists(&TenantId::default(), "Order", "order-a"),
        "the wait does not start the deleted entity again"
    );
}

#[tokio::test]
async fn entity_events_end_once_the_caller_may_no_longer_read_the_entity() {
    let state = state_with_entity_read_policy().await;
    let replayed = state
        .entity_observe_replay("default", "Order", "order-a", 0)
        .events
        .last()
        .map(|event| event.seq)
        .unwrap_or(0);
    let response = app(state.clone())
        .oneshot(agent_in(
            "default",
            "/observe/entities/Order/order-a/events",
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // Submitted orders are not readable under the policy.
    submit(&state, "order-a").await;

    let body = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024),
    )
    .await
    .expect("the stream ends once the entity is no longer readable")
    .expect("body should read");
    let body = String::from_utf8(body.to_vec()).expect("SSE is text");
    for id in body.lines().filter_map(|line| line.strip_prefix("id:")) {
        let seq: u64 = id.trim().parse().expect("event ids are sequences");
        assert!(seq <= replayed, "no event after submission is sent: {body}");
    }
}

/// Whether an actor for this entity is live in memory.
fn has_live_actor(state: &ServerState, tenant: &str, id: &str) -> bool {
    let key = format!("{tenant}:Order:{id}");
    state
        .actor_registry
        .read()
        .unwrap()
        .get(&key)
        .is_some_and(|actor| !actor.is_closed())
}

#[tokio::test]
async fn reading_an_entity_deleted_during_its_load_does_not_start_it_again() {
    let state = state_with_shared_store().await;
    let tenant = TenantId::new("tenant-a");
    let result = state
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "o-del",
            "AddItem",
            serde_json::json!({"ProductId": "p1"}),
            &AgentContext::default(),
        )
        .await;
    assert!(result.is_ok_and(|r| r.success), "AddItem failed");
    state
        .delete_tenant_entity(&tenant, "Order", "o-del")
        .await
        .expect("delete succeeds");

    for route in [
        "history",
        "wait?statuses=Shipped&timeout_ms=50&poll_ms=10",
        "events",
    ] {
        // The id is still indexed, as when the read's existence check passed
        // just before the delete finished.
        state
            .entity_index
            .write()
            .unwrap()
            .entry("tenant-a:Order".to_string())
            .or_default()
            .insert("o-del".to_string());

        let uri = format!("/observe/entities/Order/o-del/{route}");
        let response = app(state.clone())
            .oneshot(system_in("tenant-a", "GET", &uri))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        assert!(
            !has_live_actor(&state, "tenant-a", "o-del"),
            "{uri} started the deleted entity again"
        );
        assert!(
            !state.entity_exists(&tenant, "Order", "o-del"),
            "{uri} left the deleted entity indexed"
        );
    }
}
