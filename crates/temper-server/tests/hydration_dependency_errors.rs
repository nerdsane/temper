//! temper#529 P2 follow-up (issue #529): a journal read failure during
//! entity hydration (`ServerState::ensure_entity_loaded`'s not-in-registry
//! branch) must never be reported the same way as confirmed absence.
//!
//! `odata/read.rs`'s point-read path is the externally observable half of
//! the hydration seam: a non-resident entity with no catalog row falls
//! through to `ensure_entity_loaded`, and a transient `EventStore::read_events`
//! failure there must surface as a typed dependency failure (503
//! `JournalUnavailable`, matching the catalog-freshness check added for the
//! same issue in `catalog_row_validation_journal_failure_returns_dependency_error`
//! in `fresh_point_reads.rs`), never a fabricated 404.
//!
//! Uses `SimEventStore` fault injection (`fail_next_reads`) for
//! deterministic, in-memory journal failures.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use common::{build_default_state_with_store, dispatch};
use temper_authz::{AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext};
use temper_runtime::persistence::EventStore;
use temper_runtime::tenant::TenantId;
use temper_server::ServerState;
use temper_server::build_router;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

/// Permit `read`/`list` on every `Order` for any principal.
const ORDER_READ_POLICY: &str = r#"
    permit(
        principal,
        action in [Action::"list", Action::"read"],
        resource is Order
    );
"#;

fn install_read_policy(state: &ServerState, tenant: &str) {
    state
        .authz
        .reload_tenant_policies(tenant, ORDER_READ_POLICY)
        .expect("install read policy");
}

fn authenticate(mut request: Request<Body>, principal_id: &str) -> Request<Body> {
    let security_context = SecurityContext {
        principal: Principal {
            id: principal_id.to_string(),
            kind: PrincipalKind::Customer,
            role: None,
            acting_for: None,
            agent_type: None,
            attributes: Default::default(),
        },
        context_attrs: Default::default(),
        correlation_id: "hydration-dependency-errors-test".to_string(),
    };
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::default(),
            security_context,
        ));
    request
}

async fn get_json(state: &ServerState, path: &str) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let req = authenticate(
        Request::builder().uri(path).body(Body::empty()).unwrap(),
        "test-customer",
    );
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// RED: a cold reader with no catalog row and no resident actor for an
/// entity that genuinely exists must get a typed dependency failure when
/// the journal read backing `ensure_entity_loaded` fails — never the same
/// 404 a never-created id gets.
#[tokio::test]
async fn point_read_journal_failure_for_non_resident_entity_is_dependency_error_not_404() {
    let tenant = TenantId::default();
    let store = SimEventStore::no_faults(529_201);
    let entity_id = "ord-hydration-fail-a";

    let writer = build_default_state_with_store(store.clone(), "hydration-dep-writer");
    dispatch(
        &writer,
        &tenant,
        "Order",
        entity_id,
        "Create",
        serde_json::json!({}),
    )
    .await
    .expect("create order");

    // Cold reader: its own empty actor registry/index and no catalog store
    // at all, sharing only the durable journal — exactly the "no catalog
    // row, not resident" shape that falls through to `ensure_entity_loaded`.
    let reader = build_default_state_with_store(store.clone(), "hydration-dep-reader");
    install_read_policy(&reader, tenant.as_str());
    assert!(
        !reader.entity_exists(&tenant, "Order", entity_id),
        "precondition: reader must not already have this entity resident"
    );

    let persistence_id = format!("{tenant}:Order:{entity_id}");
    store.fail_next_reads(&persistence_id, 1);

    let (status, body) = get_json(&reader, &format!("/tdata/Orders('{entity_id}')")).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a journal read failure must surface as a typed dependency error, \
         never a silent 200 and never a fabricated 404: {body:?}"
    );
    assert_eq!(body["error"]["code"].as_str(), Some("JournalUnavailable"));
    assert!(
        !reader.entity_exists(&tenant, "Order", entity_id),
        "a dependency failure must not fabricate residency either"
    );
    assert_eq!(
        store.read_events(&persistence_id, 0).await.unwrap().len(),
        1,
        "a failed read must never mutate the journal"
    );
}

/// Control: an id with zero history is still confirmed absence — 404 —
/// when the journal is healthy, with no catalog row involved at all.
#[tokio::test]
async fn never_created_entity_without_catalog_still_returns_404() {
    let tenant = TenantId::default();
    let store = SimEventStore::no_faults(529_202);
    let state = build_default_state_with_store(store, "hydration-dep-missing");
    install_read_policy(&state, tenant.as_str());

    let (status, body) = get_json(&state, "/tdata/Orders('ord-never-created')").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body={body:?}");
}

/// Control: a tombstoned entity is still confirmed absence — 404, never
/// resurrected — independent of the journal-failure discrimination added
/// by this change.
#[tokio::test]
async fn tombstoned_entity_without_catalog_still_returns_404() {
    let tenant = TenantId::default();
    let store = SimEventStore::no_faults(529_203);
    let entity_id = "ord-hydration-tombstone-a";
    let writer = build_default_state_with_store(store.clone(), "hydration-dep-tombstone-writer");
    dispatch(
        &writer,
        &tenant,
        "Order",
        entity_id,
        "Create",
        serde_json::json!({}),
    )
    .await
    .expect("create order");
    writer
        .delete_tenant_entity(&tenant, "Order", entity_id)
        .await
        .expect("delete order");

    let reader = build_default_state_with_store(store, "hydration-dep-tombstone-reader");
    install_read_policy(&reader, tenant.as_str());

    let (status, body) = get_json(&reader, &format!("/tdata/Orders('{entity_id}')")).await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "a tombstoned entity must stay 404, never resurrected: {body:?}"
    );
}

/// Control: an entity already resident in the requesting process is
/// unaffected by a journal fault — the point-read path never calls
/// `ensure_entity_loaded` for an already-indexed entity, so there must be
/// no spurious 5xx.
#[tokio::test]
async fn resident_entity_point_read_unaffected_by_journal_fault() {
    let tenant = TenantId::default();
    let store = SimEventStore::no_faults(529_204);
    let entity_id = "ord-hydration-resident-a";
    let state = build_default_state_with_store(store.clone(), "hydration-dep-resident");
    install_read_policy(&state, tenant.as_str());

    dispatch(
        &state,
        &tenant,
        "Order",
        entity_id,
        "Create",
        serde_json::json!({}),
    )
    .await
    .expect("create order");
    assert!(state.entity_exists(&tenant, "Order", entity_id));

    let persistence_id = format!("{tenant}:Order:{entity_id}");
    store.fail_next_reads(&persistence_id, 1);

    let (status, body) = get_json(&state, &format!("/tdata/Orders('{entity_id}')")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "a resident entity's point read must not become a dependency error on a journal fault: {body:?}"
    );
}
