//! Fresh point reads (temper#529 finding V1, point-read half): a
//! journal-backed GET must never serve a catalog image older than the
//! validated authoritative sequence.
//!
//! Uses `SimEventStore` for the journal (deterministic, in-memory) paired
//! with a minimal in-memory `QueryPlaneStore` double that stands in for the
//! durable `entity_catalog` projection. The double is driven only through
//! its real `QueryPlaneStore` methods (`upsert_projection` /
//! `remove_projection`), the same API a durable backend uses, so "inject a
//! stale row" means exactly what a lagging real projection write would
//! produce.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use temper_authz::{AuthenticatedRequestContext, Principal, PrincipalKind, SecurityContext};
use temper_runtime::ActorSystem;
use temper_runtime::persistence::PersistenceError;
use temper_runtime::tenant::TenantId;
use temper_server::entity_actor::EntityState;
use temper_server::registry::SpecRegistry;
use temper_server::storage::{EntityCatalogRow, QueryPlaneStore, QueryProjectionFieldsRow};
use temper_server::{ServerState, StorageStack, build_router};
use temper_spec::csdl::parse_csdl;
use temper_store_sim::SimEventStore;
use tower::ServiceExt;

/// Minimal in-memory `QueryPlaneStore` double. Only `upsert_projection`,
/// `remove_projection`, and `load_entity_catalog_rows` are exercised by the
/// point-read path under test; every other method panics so an unexpected
/// call (e.g. a filtered-list/native-query path, out of scope for this
/// slice) fails loudly instead of silently returning an empty result.
#[derive(Default)]
struct TestCatalog {
    rows: Mutex<BTreeMap<(String, String, String), EntityCatalogRow>>,
}

impl TestCatalog {
    fn put(&self, tenant: &str, entity_type: &str, entity_id: &str, row: EntityCatalogRow) {
        self.rows.lock().unwrap().insert(
            (
                tenant.to_string(),
                entity_type.to_string(),
                entity_id.to_string(),
            ),
            row,
        );
    }

    fn row_sequence(&self, tenant: &str, entity_type: &str, entity_id: &str) -> Option<u64> {
        self.rows
            .lock()
            .unwrap()
            .get(&(
                tenant.to_string(),
                entity_type.to_string(),
                entity_id.to_string(),
            ))
            .map(|row| row.sequence_nr)
    }
}

#[async_trait]
impl QueryPlaneStore for TestCatalog {
    async fn upsert_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
        status: &str,
        fields: &serde_json::Value,
        state: &serde_json::Value,
        sequence_nr: u64,
    ) -> Result<(), PersistenceError> {
        self.put(
            tenant,
            entity_type,
            entity_id,
            EntityCatalogRow {
                entity_id: entity_id.to_string(),
                status: status.to_string(),
                fields: fields.clone(),
                state: Some(state.clone()),
                sequence_nr,
            },
        );
        Ok(())
    }

    async fn remove_projection(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_id: &str,
    ) -> Result<(), PersistenceError> {
        self.rows.lock().unwrap().remove(&(
            tenant.to_string(),
            entity_type.to_string(),
            entity_id.to_string(),
        ));
        Ok(())
    }

    async fn query_field_index(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _where_clause: &str,
        _params: Vec<String>,
    ) -> Result<Option<Vec<String>>, PersistenceError> {
        panic!("fresh point-read tests do not exercise filtered-list/native-query planning");
    }

    async fn load_projection_fields_many(
        &self,
        _tenant: &str,
        _entity_type: &str,
        _entity_ids: &[String],
        _field_names: &[&str],
    ) -> Result<Option<Vec<QueryProjectionFieldsRow>>, PersistenceError> {
        panic!("fresh point-read tests do not exercise selected-field index reads");
    }

    async fn load_entity_catalog_rows(
        &self,
        tenant: &str,
        entity_type: &str,
        entity_ids: &[String],
    ) -> Result<Option<Vec<EntityCatalogRow>>, PersistenceError> {
        let rows = self.rows.lock().unwrap();
        Ok(Some(
            entity_ids
                .iter()
                .filter_map(|id| {
                    rows.get(&(tenant.to_string(), entity_type.to_string(), id.clone()))
                        .cloned()
                })
                .collect(),
        ))
    }

    async fn projected_entity_counts_by_tenant(
        &self,
    ) -> Result<Option<Vec<(String, u64)>>, PersistenceError> {
        Ok(None)
    }
}

/// Permit `read`/`list` on every `Order` for any principal.
const ORDER_READ_POLICY: &str = r#"
    permit(
        principal,
        action in [Action::"list", Action::"read"],
        resource is Order
    );
"#;

/// Permit `read`/`list` on `Order` only for `Customer::"allowed-reader"`.
const ORDER_READ_POLICY_RESTRICTED: &str = r#"
    permit(
        principal == Customer::"allowed-reader",
        action in [Action::"list", Action::"read"],
        resource is Order
    );
"#;

fn build_state(
    system_name: &str,
    tenant: &str,
    journal: SimEventStore,
    catalog: Arc<TestCatalog>,
) -> ServerState {
    let mut registry = SpecRegistry::new();
    let csdl = parse_csdl(common::CSDL_XML).expect("CSDL parse");
    registry.register_tenant(
        tenant,
        csdl,
        common::CSDL_XML.to_string(),
        &[("Order", common::ORDER_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new(system_name), registry);
    let mut stack = StorageStack::from_sim(journal, None);
    stack.query_plane = Some(catalog as Arc<dyn QueryPlaneStore>);
    state.set_storage_stack(stack);
    state
}

fn install_policy(state: &ServerState, tenant: &str, policy: &str) {
    state
        .authz
        .reload_tenant_policies(tenant, policy)
        .expect("install test policy");
}

fn authenticate(mut request: Request<Body>, tenant: &str, principal_id: &str) -> Request<Body> {
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
        correlation_id: "fresh-point-reads-test".to_string(),
    };
    request
        .extensions_mut()
        .insert(AuthenticatedRequestContext::new(
            TenantId::new(tenant),
            security_context,
        ));
    request
}

async fn get_json(
    state: &ServerState,
    path: &str,
    tenant: &str,
    principal_id: &str,
) -> (StatusCode, serde_json::Value) {
    let router = build_router(state.clone());
    let req = authenticate(
        Request::builder().uri(path).body(Body::empty()).unwrap(),
        tenant,
        principal_id,
    );
    let resp = router.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1_000_000)
        .await
        .unwrap();
    let body = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, body)
}

/// Seed the catalog double with a row that mirrors what a real
/// `upsert_projection` call writes right after an actor transition — the
/// same shape `materialize_entity_set_entities`'s actor-fallback repair
/// writes in production.
fn seed_catalog_row(catalog: &TestCatalog, tenant: &str, entity_type: &str, state: &EntityState) {
    let state_json = serde_json::to_value(state).expect("serialize entity state");
    catalog.put(
        tenant,
        entity_type,
        &state.entity_id,
        EntityCatalogRow {
            entity_id: state.entity_id.clone(),
            status: state.status.clone(),
            fields: state.fields.clone(),
            state: Some(state_json),
            sequence_nr: state.sequence_nr,
        },
    );
}

fn add_item_params() -> serde_json::Value {
    serde_json::json!({"ProductId": "sku-1", "Quantity": 1})
}

// ── RED A: stale row, live journal ahead of it ──────────────────────────

#[tokio::test]
async fn stale_catalog_row_serves_fresh_authoritative_state() {
    let tenant = TenantId::default();
    let journal = SimEventStore::no_faults(529_001);
    let catalog = Arc::new(TestCatalog::default());
    let state = build_state(
        "fresh-reads-stale-row",
        tenant.as_str(),
        journal.clone(),
        catalog.clone(),
    );
    install_policy(&state, tenant.as_str(), ORDER_READ_POLICY);

    let entity_id = "ord-stale-a";
    let created = common::dispatch(
        &state,
        &tenant,
        "Order",
        entity_id,
        "Create",
        serde_json::json!({}),
    )
    .await
    .expect("create");
    assert_eq!(created.state.sequence_nr, 1);
    seed_catalog_row(&catalog, tenant.as_str(), "Order", &created.state);

    let updated = common::dispatch(
        &state,
        &tenant,
        "Order",
        entity_id,
        "AddItem",
        add_item_params(),
    )
    .await
    .expect("add item");
    assert_eq!(updated.state.sequence_nr, 2);
    assert_eq!(updated.state.counters.get("items").copied(), Some(1));
    // Force the drift that this slice must survive: re-inject the pre-update
    // row through the real `QueryPlaneStore::upsert_projection` API so the
    // catalog reflects sequence 1 again, regardless of whether the
    // background `QueryProjectionWriteQueue` had already caught up. This is
    // the "projection write never landed / landed late" race, not a guard
    // bypass.
    seed_catalog_row(&catalog, tenant.as_str(), "Order", &created.state);
    assert_eq!(
        catalog.row_sequence(tenant.as_str(), "Order", entity_id),
        Some(1),
        "precondition: catalog row must still be stale"
    );

    let (status, body) = get_json(
        &state,
        &format!("/tdata/Orders('{entity_id}')"),
        tenant.as_str(),
        "test-customer",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body:?}");
    assert_eq!(
        body["sequence_nr"].as_u64(),
        Some(2),
        "GET must serve the authoritative sequence, not the stale catalog row: {body:?}"
    );
    assert_eq!(
        body["counters"]["items"].as_u64(),
        Some(1),
        "fresh state must be normalization-consistent with the committed AddItem (V9): {body:?}"
    );
}

// ── RED B: stale row for a now-deleted entity ───────────────────────────

#[tokio::test]
async fn stale_catalog_row_for_deleted_entity_returns_404_without_resurrection() {
    let tenant = TenantId::default();
    let journal = SimEventStore::no_faults(529_002);
    let catalog = Arc::new(TestCatalog::default());
    let state = build_state(
        "fresh-reads-deleted",
        tenant.as_str(),
        journal.clone(),
        catalog.clone(),
    );
    install_policy(&state, tenant.as_str(), ORDER_READ_POLICY);

    let entity_id = "ord-deleted-a";
    let created = common::dispatch(
        &state,
        &tenant,
        "Order",
        entity_id,
        "Create",
        serde_json::json!({}),
    )
    .await
    .expect("create");
    seed_catalog_row(&catalog, tenant.as_str(), "Order", &created.state);

    let deleted = state
        .delete_tenant_entity(&tenant, "Order", entity_id)
        .await
        .expect("delete");
    assert!(deleted.success, "delete must succeed: {deleted:?}");

    // Re-inject the pre-delete row: a drifted/lagging catalog that never
    // observed the tombstone removal still has the old live-looking row.
    seed_catalog_row(&catalog, tenant.as_str(), "Order", &created.state);
    assert_eq!(
        catalog.row_sequence(tenant.as_str(), "Order", entity_id),
        Some(1),
        "precondition: stale pre-delete row must be present"
    );

    let (status, body) = get_json(
        &state,
        &format!("/tdata/Orders('{entity_id}')"),
        tenant.as_str(),
        "test-customer",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "deleted entity must 404, never resurrect the stale row: {body:?}"
    );
}

// ── RED C: journal read failure while validating an existing row ───────

#[tokio::test]
async fn catalog_row_validation_journal_failure_returns_dependency_error() {
    let tenant = TenantId::default();
    let journal = SimEventStore::no_faults(529_003);
    let catalog = Arc::new(TestCatalog::default());
    let state = build_state(
        "fresh-reads-journal-failure",
        tenant.as_str(),
        journal.clone(),
        catalog.clone(),
    );
    install_policy(&state, tenant.as_str(), ORDER_READ_POLICY);

    let entity_id = "ord-journal-fail-a";
    let created = common::dispatch(
        &state,
        &tenant,
        "Order",
        entity_id,
        "Create",
        serde_json::json!({}),
    )
    .await
    .expect("create");
    seed_catalog_row(&catalog, tenant.as_str(), "Order", &created.state);

    let persistence_id = format!("{tenant}:Order:{entity_id}");
    journal.fail_next_reads(&persistence_id, 1);

    let (status, body) = get_json(
        &state,
        &format!("/tdata/Orders('{entity_id}')"),
        tenant.as_str(),
        "test-customer",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a journal read failure must surface as a typed dependency error, \
         never a silent stale 200 and never a fabricated 404: {body:?}"
    );
    assert_eq!(body["error"]["code"].as_str(), Some("JournalUnavailable"));
}

// ── Controls ─────────────────────────────────────────────────────────

/// Current row (sequence matches the journal tail): still served without
/// falling back to actor hydration, i.e. no behavioral change for the
/// already-fresh case this slice must preserve.
#[tokio::test]
async fn current_catalog_row_still_serves_without_regression() {
    let tenant = TenantId::default();
    let journal = SimEventStore::no_faults(529_010);
    let catalog = Arc::new(TestCatalog::default());
    let state = build_state(
        "fresh-reads-current-row",
        tenant.as_str(),
        journal.clone(),
        catalog.clone(),
    );
    install_policy(&state, tenant.as_str(), ORDER_READ_POLICY);

    let entity_id = "ord-current-a";
    let created = common::dispatch(
        &state,
        &tenant,
        "Order",
        entity_id,
        "Create",
        serde_json::json!({}),
    )
    .await
    .expect("create");
    seed_catalog_row(&catalog, tenant.as_str(), "Order", &created.state);

    let (status, body) = get_json(
        &state,
        &format!("/tdata/Orders('{entity_id}')"),
        tenant.as_str(),
        "test-customer",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body:?}");
    assert_eq!(body["sequence_nr"].as_u64(), Some(1));
    assert_eq!(body["entity_id"].as_str(), Some(entity_id));
}

/// An id with zero history is authoritatively absent — 404, catalog or not.
#[tokio::test]
async fn never_created_entity_returns_404() {
    let tenant = TenantId::default();
    let journal = SimEventStore::no_faults(529_011);
    let catalog = Arc::new(TestCatalog::default());
    let state = build_state("fresh-reads-missing", tenant.as_str(), journal, catalog);
    install_policy(&state, tenant.as_str(), ORDER_READ_POLICY);

    let (status, body) = get_json(
        &state,
        "/tdata/Orders('ord-never-created')",
        tenant.as_str(),
        "test-customer",
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body={body:?}");
}

/// Row-level authorization keeps evaluating the authoritative (freshly
/// recovered) state, not the stale catalog row, when a stale row triggers
/// fallback.
#[tokio::test]
async fn unauthorized_reader_is_denied_even_on_freshly_recovered_state() {
    let tenant = TenantId::default();
    let journal = SimEventStore::no_faults(529_012);
    let catalog = Arc::new(TestCatalog::default());
    let state = build_state(
        "fresh-reads-authz",
        tenant.as_str(),
        journal.clone(),
        catalog.clone(),
    );
    install_policy(&state, tenant.as_str(), ORDER_READ_POLICY_RESTRICTED);

    let entity_id = "ord-authz-a";
    let created = common::dispatch(
        &state,
        &tenant,
        "Order",
        entity_id,
        "Create",
        serde_json::json!({}),
    )
    .await
    .expect("create");
    seed_catalog_row(&catalog, tenant.as_str(), "Order", &created.state);
    common::dispatch(
        &state,
        &tenant,
        "Order",
        entity_id,
        "AddItem",
        add_item_params(),
    )
    .await
    .expect("add item");
    // Force the catalog row back to stale (sequence 1), regardless of
    // whether the background projection queue already caught up: the GET
    // below must fall back to fresh actor state and authorize THAT state,
    // not the stale row.
    seed_catalog_row(&catalog, tenant.as_str(), "Order", &created.state);
    assert_eq!(
        catalog.row_sequence(tenant.as_str(), "Order", entity_id),
        Some(1),
        "precondition: catalog row must still be stale"
    );

    let (status, body) = get_json(
        &state,
        &format!("/tdata/Orders('{entity_id}')"),
        tenant.as_str(),
        "someone-else",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "an unauthorized reader must be denied even when the catalog row was stale: {body:?}"
    );

    let (status, body) = get_json(
        &state,
        &format!("/tdata/Orders('{entity_id}')"),
        tenant.as_str(),
        "allowed-reader",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body:?}");
    assert_eq!(body["sequence_nr"].as_u64(), Some(2));
}

/// Distinct tenants never see each other's catalog rows or fresh state.
#[tokio::test]
async fn distinct_tenants_are_isolated_through_the_freshness_check() {
    let journal = SimEventStore::no_faults(529_013);
    let catalog = Arc::new(TestCatalog::default());
    let mut registry = SpecRegistry::new();
    let csdl_a = parse_csdl(common::CSDL_XML).expect("CSDL parse");
    registry.register_tenant(
        "tenant-a",
        csdl_a,
        common::CSDL_XML.to_string(),
        &[("Order", common::ORDER_IOA)],
    );
    let csdl_b = parse_csdl(common::CSDL_XML).expect("CSDL parse");
    registry.register_tenant(
        "tenant-b",
        csdl_b,
        common::CSDL_XML.to_string(),
        &[("Order", common::ORDER_IOA)],
    );
    let mut state = ServerState::from_registry(ActorSystem::new("fresh-reads-tenants"), registry);
    let mut stack = StorageStack::from_sim(journal.clone(), None);
    stack.query_plane = Some(catalog.clone() as Arc<dyn QueryPlaneStore>);
    state.set_storage_stack(stack);
    install_policy(&state, "tenant-a", ORDER_READ_POLICY);
    install_policy(&state, "tenant-b", ORDER_READ_POLICY);

    let tenant_a = TenantId::new("tenant-a");
    let entity_id = "ord-shared-id";
    let created_a = common::dispatch(
        &state,
        &tenant_a,
        "Order",
        entity_id,
        "Create",
        serde_json::json!({}),
    )
    .await
    .expect("create in tenant-a");
    seed_catalog_row(&catalog, "tenant-a", "Order", &created_a.state);

    // tenant-b never created this id: must 404, not see tenant-a's row.
    let (status, body) = get_json(
        &state,
        &format!("/tdata/Orders('{entity_id}')"),
        "tenant-b",
        "test-customer",
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NOT_FOUND,
        "tenant-b must not observe tenant-a's catalog row: {body:?}"
    );

    let (status, body) = get_json(
        &state,
        &format!("/tdata/Orders('{entity_id}')"),
        "tenant-a",
        "test-customer",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body:?}");
}

/// A cold reader — a fresh `ServerState` sharing the same durable journal and
/// catalog double, with no actor cached in memory — validates against the
/// shared journal exactly like the writer process does.
#[tokio::test]
async fn cold_reader_validates_a_stale_row_against_the_shared_journal() {
    let tenant = TenantId::default();
    let journal = SimEventStore::no_faults(529_014);
    let catalog = Arc::new(TestCatalog::default());
    let writer = build_state(
        "fresh-reads-cold-writer",
        tenant.as_str(),
        journal.clone(),
        catalog.clone(),
    );

    let entity_id = "ord-cold-a";
    let created = common::dispatch(
        &writer,
        &tenant,
        "Order",
        entity_id,
        "Create",
        serde_json::json!({}),
    )
    .await
    .expect("create");
    seed_catalog_row(&catalog, tenant.as_str(), "Order", &created.state);
    common::dispatch(
        &writer,
        &tenant,
        "Order",
        entity_id,
        "AddItem",
        add_item_params(),
    )
    .await
    .expect("add item");
    // Force the catalog row back to stale (sequence 1) so the cold reader
    // genuinely has to validate against the shared journal, not merely
    // observe an already-caught-up background projection write.
    seed_catalog_row(&catalog, tenant.as_str(), "Order", &created.state);
    assert_eq!(
        catalog.row_sequence(tenant.as_str(), "Order", entity_id),
        Some(1),
        "precondition: catalog row must still be stale"
    );

    // A second ServerState over the same journal + catalog, with its own
    // empty actor registry: the "cold reader" scenario.
    let reader = build_state("fresh-reads-cold-reader", tenant.as_str(), journal, catalog);
    install_policy(&reader, tenant.as_str(), ORDER_READ_POLICY);

    let (status, body) = get_json(
        &reader,
        &format!("/tdata/Orders('{entity_id}')"),
        tenant.as_str(),
        "test-customer",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body:?}");
    assert_eq!(
        body["sequence_nr"].as_u64(),
        Some(2),
        "a cold reader must still prove the catalog row current before serving it: {body:?}"
    );
}

/// `$select` behavior is unchanged by the freshness check — selecting a
/// field on a stale-then-recovered entity returns only that field.
#[tokio::test]
async fn select_fields_unchanged_when_catalog_row_is_stale() {
    let tenant = TenantId::default();
    let journal = SimEventStore::no_faults(529_015);
    let catalog = Arc::new(TestCatalog::default());
    let state = build_state(
        "fresh-reads-select",
        tenant.as_str(),
        journal.clone(),
        catalog.clone(),
    );
    install_policy(&state, tenant.as_str(), ORDER_READ_POLICY);

    let entity_id = "ord-select-a";
    let created = common::dispatch(
        &state,
        &tenant,
        "Order",
        entity_id,
        "Create",
        serde_json::json!({}),
    )
    .await
    .expect("create");
    seed_catalog_row(&catalog, tenant.as_str(), "Order", &created.state);
    common::dispatch(
        &state,
        &tenant,
        "Order",
        entity_id,
        "AddItem",
        add_item_params(),
    )
    .await
    .expect("add item");
    // Force the catalog row back to stale (sequence 1); $select must still
    // return the right fields off the freshly recovered state.
    seed_catalog_row(&catalog, tenant.as_str(), "Order", &created.state);

    let (status, body) = get_json(
        &state,
        &format!("/tdata/Orders('{entity_id}')?$select=entity_id"),
        tenant.as_str(),
        "test-customer",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body={body:?}");
    assert_eq!(body["entity_id"].as_str(), Some(entity_id));
}
