//! Authenticated HTTP repair workflow for ARN-190 quarantines.
#![cfg(feature = "observe")]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::ActorSystem;
use temper_runtime::tenant::TenantId;
use temper_server::build_router;
use temper_server::identity::ResolvedIdentity;
use temper_server::registry::SpecRegistry;
use temper_server::registry_bootstrap::restore_registry_from_turso;
use temper_server::state::ServerState;
use temper_server::storage::StorageStack;
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
const ORDER_CSDL: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");

fn admin_request(method: &str, uri: &str) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-temper-principal-id", "registry-operator")
        .header("x-temper-principal-kind", "admin")
        .body(Body::empty())
        .expect("admin request");
    request
        .extensions_mut()
        .insert(temper_server::authz::TrustedIngressPrincipal);
    request
}

fn admin_json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .header("x-temper-principal-id", "registry-operator")
        .header("x-temper-principal-kind", "admin")
        .body(Body::from(body.to_string()))
        .expect("admin JSON request");
    request
        .extensions_mut()
        .insert(temper_server::authz::TrustedIngressPrincipal);
    request
}

async fn response_json(response: axum::response::Response) -> serde_json::Value {
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .expect("bounded response body");
    serde_json::from_slice(&body).expect("JSON response")
}

#[tokio::test]
async fn quarantine_management_api_lists_acknowledges_retries_and_recovers() {
    let directory = tempfile::tempdir().expect("temporary API Turso directory");
    let url = format!(
        "file:{}",
        directory.path().join("registry-api.db").display()
    );
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("create API Turso adapter");
    store
        .upsert_spec("broken", "Order", ORDER_IOA, "<a><b", "broken-v1")
        .await
        .expect("seed API corrupt spec");
    store
        .commit_specs("broken")
        .await
        .expect("commit API corrupt spec");
    let mut registry = SpecRegistry::new();
    restore_registry_from_turso(&mut registry, &store)
        .await
        .expect("restore API registry");
    let mut state = ServerState::from_registry(ActorSystem::new("registry-api-e2e"), registry);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let app = build_router(state.clone());

    let list = app
        .clone()
        .oneshot(admin_request(
            "GET",
            "/api/tenants/broken/registry-quarantines",
        ))
        .await
        .expect("list request");
    assert_eq!(list.status(), StatusCode::OK);
    let list = response_json(list).await;
    assert_eq!(list["total"], 1);
    assert_eq!(list["records"][0]["reason"], "invalid_csdl");
    assert!(
        list["records"][0]["detail"]
            .as_str()
            .is_some_and(|detail| !detail.is_empty()),
        "authenticated repair API must include bounded diagnostics"
    );

    let wrong_entity = app
        .clone()
        .oneshot(admin_request(
            "POST",
            "/api/tenants/broken/registry-quarantines/NotQuarantined/retry",
        ))
        .await
        .expect("wrong-entity retry request");
    assert_eq!(
        wrong_entity.status(),
        StatusCode::NOT_FOUND,
        "the route entity must identify a local or durable quarantine record"
    );

    let denied = app
        .clone()
        .oneshot(
            Request::post("/api/tenants/broken/registry-quarantines/Order/acknowledge")
                .header("x-temper-principal-id", "spoofed-operator")
                .header("x-temper-principal-kind", "admin")
                .body(Body::empty())
                .expect("spoofed request"),
        )
        .await
        .expect("spoofed acknowledge");
    assert_eq!(denied.status(), StatusCode::FORBIDDEN);

    let mut agent_spoof = admin_request(
        "POST",
        "/api/tenants/broken/registry-quarantines/Order/acknowledge",
    );
    agent_spoof.extensions_mut().insert(ResolvedIdentity {
        agent_instance_id: "agent-instance".to_string(),
        agent_type_id: "agent-type".to_string(),
        agent_type_name: "review-agent".to_string(),
        verified: true,
    });
    let denied_agent = app
        .clone()
        .oneshot(agent_spoof)
        .await
        .expect("agent credential spoof request");
    assert_eq!(
        denied_agent.status(),
        StatusCode::FORBIDDEN,
        "a resolved agent credential cannot escalate through principal headers"
    );

    let acknowledged = app
        .clone()
        .oneshot(admin_json_request(
            "POST",
            "/api/tenants/broken/registry-quarantines/Order/acknowledge",
            serde_json::json!({"spec_version": 1, "constraint_version": null}),
        ))
        .await
        .expect("acknowledge request");
    assert_eq!(acknowledged.status(), StatusCode::NO_CONTENT);
    let health_after_acknowledgment = app
        .clone()
        .oneshot(
            Request::get("/observe/health")
                .body(Body::empty())
                .expect("health request"),
        )
        .await
        .expect("health after acknowledgment");
    assert_eq!(
        response_json(health_after_acknowledgment).await["registry_restore"]["quarantined_tenants"]
            ["broken"]["entity_failures"]["Order"]["acknowledged"],
        true,
        "durable acknowledgment must be visible in same-process health immediately"
    );

    let still_bad = app
        .clone()
        .oneshot(admin_request(
            "POST",
            "/api/tenants/broken/registry-quarantines/Order/retry",
        ))
        .await
        .expect("failed retry request");
    assert_eq!(still_bad.status(), StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        state
            .registry
            .read()
            .expect("health lock")
            .restore_health()
            .quarantined_tenants["broken"]
            .entity_failures["Order"]
            .acknowledged,
        "refreshing the same failed identity must preserve durable acknowledgment"
    );

    let stale_registry = state
        .registry
        .read()
        .expect("stale replica source lock")
        .clone();
    let mut stale_state = ServerState::from_registry(
        ActorSystem::new("registry-api-stale-ack-replica"),
        stale_registry,
    );
    stale_state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let stale_app = build_router(stale_state.clone());

    store
        .upsert_spec("broken", "Order", ORDER_IOA, "<still><broken", "broken-v2")
        .await
        .expect("advance API corrupt spec identity");
    store
        .commit_specs("broken")
        .await
        .expect("commit advanced API corrupt spec");
    let advanced_failure = app
        .clone()
        .oneshot(admin_request(
            "POST",
            "/api/tenants/broken/registry-quarantines/Order/retry",
        ))
        .await
        .expect("advanced failed retry request");
    assert_eq!(advanced_failure.status(), StatusCode::UNPROCESSABLE_ENTITY);

    let stale_acknowledgment = app
        .clone()
        .oneshot(admin_json_request(
            "POST",
            "/api/tenants/broken/registry-quarantines/Order/acknowledge",
            serde_json::json!({"spec_version": 1, "constraint_version": null}),
        ))
        .await
        .expect("stale acknowledgment request");
    assert_eq!(stale_acknowledgment.status(), StatusCode::CONFLICT);
    let advanced_list = app
        .clone()
        .oneshot(admin_request(
            "GET",
            "/api/tenants/broken/registry-quarantines",
        ))
        .await
        .expect("advanced list request");
    let advanced_list = response_json(advanced_list).await;
    assert_eq!(advanced_list["records"][0]["spec_version"], 2);
    assert!(advanced_list["records"][0]["acknowledged_at"].is_null());

    let stale_replica_current_ack = stale_app
        .oneshot(admin_json_request(
            "POST",
            "/api/tenants/broken/registry-quarantines/Order/acknowledge",
            serde_json::json!({"spec_version": 2, "constraint_version": null}),
        ))
        .await
        .expect("stale replica current acknowledgment");
    assert_eq!(stale_replica_current_ack.status(), StatusCode::NO_CONTENT);
    let (stale_spec_version, stale_acknowledged) = {
        let stale_registry = stale_state
            .registry
            .read()
            .expect("reconciled stale replica lock");
        let stale_failure =
            &stale_registry.restore_health().quarantined_tenants["broken"].entity_failures["Order"];
        (stale_failure.spec_version, stale_failure.acknowledged)
    };
    assert_eq!(stale_spec_version, 2);
    assert!(
        stale_acknowledged,
        "durable current identity must replace an older local acknowledgment identity"
    );

    let current_acknowledgment = app
        .clone()
        .oneshot(admin_json_request(
            "POST",
            "/api/tenants/broken/registry-quarantines/Order/acknowledge",
            serde_json::json!({"spec_version": 2, "constraint_version": null}),
        ))
        .await
        .expect("current acknowledgment request");
    assert_eq!(current_acknowledgment.status(), StatusCode::NO_CONTENT);

    let mut second_registry = SpecRegistry::new();
    restore_registry_from_turso(&mut second_registry, &store)
        .await
        .expect("restore second replica registry");
    assert!(
        second_registry.restore_health().quarantined_tenants["broken"].entity_failures["Order"]
            .acknowledged,
        "a new replica must hydrate acknowledgment from durable quarantine state"
    );
    let mut second_state = ServerState::from_registry(
        ActorSystem::new("registry-api-second-replica"),
        second_registry,
    );
    second_state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let second_app = build_router(second_state.clone());

    store
        .upsert_spec("broken", "Order", ORDER_IOA, ORDER_CSDL, "repaired-v3")
        .await
        .expect("repair API persisted source");
    store
        .commit_specs("broken")
        .await
        .expect("commit API repaired source");
    let repaired = app
        .clone()
        .oneshot(admin_request(
            "POST",
            "/api/tenants/broken/registry-quarantines/Order/retry",
        ))
        .await
        .expect("successful retry request");
    assert_eq!(repaired.status(), StatusCode::OK);
    assert!(
        state
            .registry
            .read()
            .expect("live registry lock")
            .get_table(&TenantId::new("broken"), "Order")
            .is_some(),
        "repair endpoint must activate the repaired tenant"
    );
    assert!(
        second_state
            .registry
            .read()
            .expect("second live registry lock")
            .restore_health()
            .is_quarantined("broken", "Order"),
        "the second ServerState must begin this step with stale local quarantine health"
    );
    let second_repaired = second_app
        .oneshot(admin_request(
            "POST",
            "/api/tenants/broken/registry-quarantines/Order/retry",
        ))
        .await
        .expect("second replica retry request");
    assert_eq!(
        second_repaired.status(),
        StatusCode::OK,
        "an exact durable resolution must let a stale replica converge without restart"
    );
    assert!(
        second_state
            .registry
            .read()
            .expect("second repaired registry lock")
            .get_table(&TenantId::new("broken"), "Order")
            .is_some()
    );

    let resolved = app
        .oneshot(admin_request(
            "GET",
            "/api/tenants/broken/registry-quarantines",
        ))
        .await
        .expect("resolved list request");
    assert_eq!(response_json(resolved).await["total"], 0);
}

#[tokio::test]
async fn replica_without_local_quarantine_reconciles_from_durable_state() {
    let directory = tempfile::tempdir().expect("temporary replica Turso directory");
    let url = format!(
        "file:{}",
        directory.path().join("registry-replica.db").display()
    );
    let store = TursoEventStore::new(&url, None)
        .await
        .expect("create replica Turso adapter");

    // Replica B starts before the bad committed source exists, so its local
    // registry health is healthy and contains no quarantine identity.
    let mut replica_b_state = ServerState::from_registry(
        ActorSystem::new("registry-api-replica-b"),
        SpecRegistry::new(),
    );
    replica_b_state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let replica_b_app = build_router(replica_b_state.clone());
    let mut replica_c_state = ServerState::from_registry(
        ActorSystem::new("registry-api-replica-c"),
        SpecRegistry::new(),
    );
    replica_c_state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let replica_c_app = build_router(replica_c_state.clone());

    store
        .upsert_spec("later-broken", "Order", ORDER_IOA, "<a><b", "broken-v1")
        .await
        .expect("seed later corrupt spec");
    store
        .commit_specs("later-broken")
        .await
        .expect("commit later corrupt spec");

    // Replica A observes the source and opens the durable quarantine shared by
    // both processes. Replica B deliberately remains stale and locally healthy.
    let mut replica_a_registry = SpecRegistry::new();
    restore_registry_from_turso(&mut replica_a_registry, &store)
        .await
        .expect("replica A restore");
    assert!(
        replica_a_registry
            .restore_health()
            .is_quarantined("later-broken", "Order")
    );
    assert!(
        replica_b_state
            .registry
            .read()
            .expect("replica B registry lock")
            .restore_health()
            .is_healthy()
    );

    let acknowledged = replica_c_app
        .clone()
        .oneshot(admin_json_request(
            "POST",
            "/api/tenants/later-broken/registry-quarantines/Order/acknowledge",
            serde_json::json!({"spec_version": 1, "constraint_version": null}),
        ))
        .await
        .expect("locally healthy replica acknowledgment");
    assert_eq!(acknowledged.status(), StatusCode::NO_CONTENT);
    let (replica_c_spec_version, replica_c_acknowledged) = {
        let replica_c_registry = replica_c_state
            .registry
            .read()
            .expect("reconciled replica C registry lock");
        let replica_c_failure =
            &replica_c_registry.restore_health().quarantined_tenants["later-broken"]
                .entity_failures["Order"];
        (
            replica_c_failure.spec_version,
            replica_c_failure.acknowledged,
        )
    };
    assert_eq!(replica_c_spec_version, 1);
    assert!(replica_c_acknowledged);
    let replica_c_health = replica_c_app
        .oneshot(
            Request::get("/observe/health")
                .body(Body::empty())
                .expect("replica C health request"),
        )
        .await
        .expect("replica C health response");
    let replica_c_health = response_json(replica_c_health).await;
    assert_eq!(replica_c_health["status"], "degraded");
    assert_eq!(
        replica_c_health["registry_restore"]["quarantined_tenants"]["later-broken"]["entity_failures"]
            ["Order"]["acknowledged"],
        true
    );

    let retry = replica_b_app
        .oneshot(admin_request(
            "POST",
            "/api/tenants/later-broken/registry-quarantines/Order/retry",
        ))
        .await
        .expect("stale healthy replica retry");
    assert_eq!(
        retry.status(),
        StatusCode::UNPROCESSABLE_ENTITY,
        "durable quarantine must reach reconciliation instead of a local-only 404"
    );
    assert!(
        replica_b_state
            .registry
            .read()
            .expect("reconciled replica B registry lock")
            .restore_health()
            .is_quarantined("later-broken", "Order"),
        "failed durable retry must reconcile process-local quarantine health"
    );
}
