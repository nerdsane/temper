//! Restoring the registry at startup, one tenant at a time.
//!
//! A tenant whose stored constraints or CSDL do not load is left out and
//! marked unavailable; every other tenant is restored, wherever the tenants
//! sort. The marked tenant's requests answer 503 with that state.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_runtime::{ActorSystem, tenant::TenantId};
use temper_server::registry::SpecRegistry;
use temper_server::registry_bootstrap::restore_registry_from_turso;
use temper_server::{ServerState, build_router};
use temper_store_turso::TursoEventStore;
use tower::ServiceExt;

const CSDL_XML: &str = include_str!("../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");
const HEALTHY: &str = "healthy-b";

async fn store() -> (tempfile::TempDir, TursoEventStore) {
    let dir = tempfile::tempdir().unwrap();
    let db_url = format!("file:{}", dir.path().join("temper.db").display());
    let store = TursoEventStore::new(&db_url, None).await.unwrap();
    (dir, store)
}

/// Commit the Order spec for `tenant` with `csdl`, as an install does.
async fn commit_order(store: &TursoEventStore, tenant: &str, csdl: &str) {
    let hash = temper_store_turso::spec_content_hash(ORDER_IOA);
    store
        .upsert_specs_and_commit(tenant, &[("Order", ORDER_IOA, csdl, &hash)], None, "orders")
        .await
        .unwrap();
}

async fn restore(store: &TursoEventStore) -> SpecRegistry {
    let mut registry = SpecRegistry::new();
    restore_registry_from_turso(&mut registry, store)
        .await
        .expect("one tenant's rows do not stop the restore");
    registry
}

fn assert_only_bad_tenant_left_out(registry: &SpecRegistry, bad: &str) {
    assert!(
        registry
            .get_table(&TenantId::new(HEALTHY), "Order")
            .is_some(),
        "the healthy tenant is restored"
    );
    assert!(registry.get_tenant(&TenantId::new(bad)).is_none());
    assert!(
        registry
            .tenant_unavailable_reason(&TenantId::new(bad))
            .is_some(),
        "the tenant whose rows did not load is marked unavailable"
    );
    assert!(
        registry
            .tenant_unavailable_reason(&TenantId::new(HEALTHY))
            .is_none()
    );
}

#[tokio::test]
async fn unloadable_constraints_leave_every_other_tenant_restored() {
    for bad in ["aaa-constraints", "zzz-constraints"] {
        let (_dir, store) = store().await;
        commit_order(&store, HEALTHY, CSDL_XML).await;
        commit_order(&store, bad, CSDL_XML).await;
        store
            .upsert_tenant_constraints(bad, "this is [[[ not toml")
            .await
            .unwrap();

        let registry = restore(&store).await;
        assert_only_bad_tenant_left_out(&registry, bad);
    }
}

#[tokio::test]
async fn unloadable_csdl_leaves_every_other_tenant_restored() {
    let (_dir, store) = store().await;
    commit_order(&store, HEALTHY, CSDL_XML).await;
    commit_order(&store, "aaa-csdl", "<not-csdl").await;

    let registry = restore(&store).await;
    assert_only_bad_tenant_left_out(&registry, "aaa-csdl");
}

async fn get_orders(state: &ServerState, tenant: &str) -> StatusCode {
    let mut request = Request::get("/tdata/Orders").body(Body::empty()).unwrap();
    request
        .extensions_mut()
        .insert(temper_authz::AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            temper_authz::SecurityContext {
                principal: temper_authz::Principal {
                    id: "operator".to_string(),
                    kind: temper_authz::PrincipalKind::Admin,
                    role: None,
                    acting_for: None,
                    agent_type: None,
                    label: None,
                    attributes: Default::default(),
                },
                context_attrs: Default::default(),
                correlation_id: "restore-isolation-test".to_string(),
                cause: None,
                credential: None,
            },
        ));
    build_router(state.clone())
        .oneshot(request)
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn an_unavailable_tenant_answers_503_and_the_others_answer_normally() {
    let mut registry = SpecRegistry::new();
    registry.mark_tenant_unavailable("aaa-constraints", "stored constraints".to_string());
    let state = ServerState::from_registry(ActorSystem::new("restore-isolation"), registry);
    state
        .authz
        .reload_tenant_policies(HEALTHY, "permit(principal, action, resource);")
        .unwrap();

    assert_eq!(
        get_orders(&state, "aaa-constraints").await,
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_ne!(
        get_orders(&state, HEALTHY).await,
        StatusCode::SERVICE_UNAVAILABLE
    );
}
