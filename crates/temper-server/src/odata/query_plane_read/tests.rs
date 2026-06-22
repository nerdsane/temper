use super::types::{
    QueryPlaneFallbackReason, QueryPlaneReadBudget, QueryPlaneReadError, QueryPlaneReadRequest,
    QueryPlaneReadStrategy,
};
use super::*;
use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
use crate::storage::{CatalogRowsLoad, load_catalog_rows_by_id};
use crate::{ServerState, StorageStack};
use temper_authz::SecurityContext;
use temper_odata::query::types::{
    BinaryOperator, FilterExpr, ODataValue, OrderByClause, OrderDirection, QueryOptions,
};
use temper_runtime::ActorSystem;
use temper_runtime::scheduler::sim_uuid;
use temper_runtime::tenant::TenantId;
use temper_spec::csdl::parse_csdl;
use temper_store_turso::TursoEventStore;

mod proof;

const CSDL_XML: &str = include_str!("../../../../../test-fixtures/specs/model.csdl.xml");
const ORDER_IOA: &str = include_str!("../../../../../test-fixtures/specs/order.ioa.toml");

fn build_order_state(system_name: &str) -> ServerState {
    let csdl = parse_csdl(CSDL_XML).expect("CSDL should parse");
    let mut registry = SpecRegistry::new();
    registry.register_tenant(
        TenantId::default().as_str(),
        csdl,
        CSDL_XML.to_string(),
        &[("Order", ORDER_IOA)],
    );
    ServerState::from_registry(ActorSystem::new(system_name), registry)
}

async fn create_orders(state: &ServerState, count: usize) {
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("query-plane-read-test");
    for index in 0..count {
        state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                &format!("ord-{index:02}"),
                "Create",
                serde_json::json!({}),
                &agent_ctx,
            )
            .await
            .expect("create order");
    }
}

async fn upsert_order_projection(
    store: &TursoEventStore,
    tenant: &TenantId,
    entity_id: &str,
    mut fields: serde_json::Value,
    sequence_nr: u64,
) {
    fields["Id"] = serde_json::json!(entity_id);
    let state_json = serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": "Created",
        "fields": fields,
        "sequence_nr": sequence_nr,
        "events": [],
    });
    store
        .upsert_query_projection_with_state(
            tenant.as_str(),
            "Order",
            entity_id,
            "Created",
            state_json.get("fields").unwrap(),
            &state_json,
            sequence_nr,
        )
        .await
        .expect("upsert projection");
}

#[test]
fn fallback_reason_labels_are_stable() {
    assert_eq!(QueryPlaneFallbackReason::None.as_str(), "none");
    assert_eq!(
        QueryPlaneFallbackReason::NoFilterPushdown.as_str(),
        "no_filter_pushdown"
    );
    assert_eq!(
        QueryPlaneFallbackReason::NativePageUnavailable.as_str(),
        "native_page_unavailable"
    );
    assert_eq!(
        QueryPlaneFallbackReason::FilterPushdownUnavailable.as_str(),
        "filter_pushdown_unavailable"
    );
    assert_eq!(
        QueryPlaneFallbackReason::CatalogCoverageGap.as_str(),
        "catalog_coverage_gap"
    );
    assert_eq!(
        QueryPlaneFallbackReason::FallbackCandidateBudget.as_str(),
        "fallback_candidate_budget"
    );
}

#[test]
fn scan_candidate_budget_matches_existing_odata_cap() {
    let small_default = QueryPlaneReadBudget {
        default_page_size: 100,
        max_entities: 1000,
    };
    assert_eq!(small_default.scan_candidate_budget(), 10_000);

    let large_default = QueryPlaneReadBudget {
        default_page_size: 20_000,
        max_entities: 1000,
    };
    assert_eq!(large_default.scan_candidate_budget(), 20_000);
}

#[test]
fn native_catalog_coverage_short_circuit_requires_pushdown_without_count() {
    let state = build_order_state("query-plane-native-coverage-short-circuit");
    let tenant = TenantId::default();
    let security_ctx = SecurityContext::system();
    let filtered_options = QueryOptions {
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Id".to_string())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(
                "ord-01".to_string(),
            ))),
        }),
        top: Some(1),
        ..QueryOptions::default()
    };
    let filtered_request = QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &filtered_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 1,
            max_entities: 10,
        },
    };
    let filtered_plan = native_candidate_page_plan(&filtered_request).expect("native plan");

    assert!(should_try_native_before_catalog_coverage(
        &filtered_request,
        &filtered_plan
    ));

    let count_options = QueryOptions {
        count: Some(true),
        ..filtered_options.clone()
    };
    let count_request = QueryPlaneReadRequest {
        query_options: &count_options,
        ..filtered_request
    };
    let count_plan = native_candidate_page_plan(&count_request).expect("native count plan");
    assert!(!should_try_native_before_catalog_coverage(
        &count_request,
        &count_plan
    ));

    let unfiltered_options = QueryOptions {
        top: Some(1),
        ..QueryOptions::default()
    };
    let unfiltered_request = QueryPlaneReadRequest {
        query_options: &unfiltered_options,
        ..count_request
    };
    let unfiltered_plan = native_candidate_page_plan(&unfiltered_request).expect("native plan");
    assert!(!should_try_native_before_catalog_coverage(
        &unfiltered_request,
        &unfiltered_plan
    ));
}

#[test]
fn source_cursor_catalog_coverage_is_skipped_when_candidate_set_exceeds_budget() {
    let budget = QueryPlaneReadBudget {
        default_page_size: 1,
        max_entities: 1,
    };

    assert!(should_check_source_cursor_catalog_coverage(
        budget.scan_candidate_budget(),
        budget
    ));
    assert!(!should_check_source_cursor_catalog_coverage(
        budget.scan_candidate_budget() + 1,
        budget
    ));
}

#[tokio::test]
async fn row_authorized_count_over_budget_returns_413() {
    let state = build_order_state("query-plane-budget-count");
    create_orders(&state, 11).await;
    let tenant = TenantId::default();
    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        count: Some(true),
        ..QueryOptions::default()
    };

    let error = match read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 1,
            max_entities: 1,
        },
    })
    .await
    {
        Ok(_) => panic!("oversized exact count should be rejected"),
        Err(error) => error,
    };

    match error {
        QueryPlaneReadError::QueryTooLarge { telemetry } => {
            assert_eq!(
                telemetry.fallback_reason,
                QueryPlaneFallbackReason::FallbackCandidateBudget
            );
            assert_eq!(telemetry.candidate_count, 11);
        }
        QueryPlaneReadError::AuthorizationDenied(_) => {
            panic!("test state should allow collection reads")
        }
    }
}

#[tokio::test]
async fn row_authorized_first_page_can_stop_after_proof() {
    let state = build_order_state("query-plane-budget-first-page");
    create_orders(&state, 11).await;
    let tenant = TenantId::default();
    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        top: Some(1),
        ..QueryOptions::default()
    };

    let result = match read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 1,
            max_entities: 1,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("first page should be provable without scanning every row"),
    };

    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.count, None);
    assert_eq!(result.telemetry.candidate_count, 1);
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::ReadSourceCursor
    );
}

#[tokio::test]
async fn projection_lagged_actor_write_repairs_missing_catalog_row() {
    let db_path =
        std::env::temp_dir().join(format!("temper-query-plane-lag-repair-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = build_order_state("query-plane-lag-repair");
    create_orders(&state, 1).await;
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let tenant = TenantId::default();

    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Id".to_string())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(
                "ord-00".to_string(),
            ))),
        }),
        top: Some(1),
        select: Some(vec!["Id".to_string(), "Status".to_string()]),
        ..QueryOptions::default()
    };

    let result = match read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 1,
            max_entities: 10,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("lagged projection should fall back to actor and repair"),
    };

    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["Id"].as_str(), Some("ord-00"));
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::ReadSourceCursor
    );
    assert_eq!(
        result.telemetry.fallback_reason,
        QueryPlaneFallbackReason::CatalogCoverageGap
    );
    assert_eq!(result.telemetry.coverage.missing, 1);

    let ids = vec!["ord-00".to_string()];
    let query_plane = state
        .query_plane_store()
        .expect("query-plane store should be configured");
    let repaired = load_catalog_rows_by_id(&query_plane, tenant.as_str(), "Order", &ids)
        .await
        .expect("load repaired catalog row");
    let CatalogRowsLoad::Available(rows) = repaired else {
        panic!("turso catalog row load should be supported");
    };
    assert!(rows.contains_key("ord-00"));

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn turso_native_pages_order_and_count_inside_query_plane() {
    let db_path =
        std::env::temp_dir().join(format!("temper-query-plane-native-page-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = build_order_state("query-plane-native-page");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let tenant = TenantId::default();

    for total in [1_u64, 10, 2] {
        let entity_id = format!("ord-{total:02}");
        upsert_order_projection(
            &store,
            &tenant,
            &entity_id,
            serde_json::json!({
            "Total": total,
            "Currency": "USD",
            }),
            total,
        )
        .await;
    }

    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        orderby: Some(vec![OrderByClause {
            property: "Id".to_string(),
            direction: OrderDirection::Desc,
        }]),
        top: Some(1),
        count: Some(true),
        select: Some(vec!["Id".to_string(), "Total".to_string()]),
        ..QueryOptions::default()
    };

    let result = match read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 2,
            max_entities: 2,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("native page read should succeed"),
    };

    assert_eq!(result.count, Some(3));
    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["Id"].as_str(), Some("ord-10"));
    assert_eq!(result.entities[0]["Total"].as_u64(), Some(10));
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::NativePagePushdown
    );
    assert!(result.telemetry.pushdown_sparse_page);
    assert!(!result.telemetry.catalog_select_projection);
    assert!(result.telemetry.select_requested);

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn native_pages_recheck_filter_before_counting_or_returning() {
    let db_path =
        std::env::temp_dir().join(format!("temper-query-plane-filter-proof-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = build_order_state("query-plane-filter-proof");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let tenant = TenantId::default();

    for total in [2_u64, 10, 20] {
        let entity_id = format!("ord-{total:02}");
        upsert_order_projection(
            &store,
            &tenant,
            &entity_id,
            serde_json::json!({ "Total": total }),
            total,
        )
        .await;
    }

    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Total".to_string())),
            op: BinaryOperator::Gt,
            right: Box::new(FilterExpr::Literal(ODataValue::Int(10))),
        }),
        count: Some(true),
        select: Some(vec!["Id".to_string(), "Total".to_string()]),
        ..QueryOptions::default()
    };

    let result = match read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 2,
            max_entities: 10,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("native candidate page read should succeed"),
    };

    assert_eq!(result.count, Some(1));
    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["Id"].as_str(), Some("ord-20"));
    assert_eq!(result.entities[0]["Total"].as_u64(), Some(20));
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::NativePagePushdown
    );
    assert!(!result.telemetry.filter_pushdown);

    let _ = std::fs::remove_file(db_path);
}

// --- ARN-89: exact-match reads are consistent under projection lag ---

fn build_order_state_with_store(system_name: &str, store: &TursoEventStore) -> ServerState {
    let mut state = build_order_state(system_name);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    state
}

fn id_eq_filter(entity_id: &str) -> FilterExpr {
    FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Id".to_string())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String(
            entity_id.to_string(),
        ))),
    }
}

/// ARN-89: an exact-match resolution must surface a just-committed entity even
/// when its asynchronously-projected row has not yet landed.
///
/// Reproduces the NEW case the fix covers: another Order is present in BOTH the
/// in-memory index and the turso projection (so catalog `coverage.missing == 0`),
/// while the TARGET Order is durable in the journal — reachable only via
/// `list_entity_ids_lazy` — and absent from the in-memory index AND the
/// projection. The pre-fix code trusted the empty native page (since coverage
/// looked complete for the indexed Order) and returned empty. The fix detects
/// the pure equality conjunction, distrusts the empty page, and reconciles
/// against authoritative state.
#[tokio::test]
async fn exact_match_read_is_consistent_when_projection_queued() {
    let db_path =
        std::env::temp_dir().join(format!("temper-query-plane-exact-lag-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("query-plane-exact-lag");

    // Writer process: commit both Orders durably to the shared journal.
    let writer = build_order_state_with_store("query-plane-exact-lag-writer", &store);
    for entity_id in ["ord-present", "ord-target"] {
        writer
            .dispatch_tenant_action(
                &tenant,
                "Order",
                entity_id,
                "Create",
                serde_json::json!({}),
                &agent_ctx,
            )
            .await
            .expect("create order");
    }

    // Make the durable catalog/projection state explicit and deterministic
    // (independent of the writer's async projection queue):
    //  - `ord-present` IS projected  -> coverage.missing == 0 for the index.
    //  - `ord-target`  is NOT projected -> only reachable via the journal.
    upsert_order_projection(
        &store,
        &tenant,
        "ord-present",
        serde_json::json!({ "Status": "Draft" }),
        1,
    )
    .await;
    store
        .remove_query_projection(tenant.as_str(), "Order", "ord-target")
        .await
        .expect("evict target projection to simulate projection lag");

    // Fresh reader process: empty in-memory index (a server restart). Touch only
    // `ord-present` so the index holds exactly that one Order, while `ord-target`
    // stays journal-only and out of the index.
    let mut state = build_order_state("query-plane-exact-lag-reader");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    assert!(
        state
            .ensure_entity_loaded(&tenant, "Order", "ord-present")
            .await,
        "ord-present should hydrate from the durable store"
    );
    assert_eq!(
        state.list_entity_ids(&tenant, "Order"),
        vec!["ord-present".to_string()],
        "precondition: in-memory index holds only the projected Order, NOT the target"
    );

    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(id_eq_filter("ord-target")),
        top: Some(1),
        select: Some(vec!["Id".to_string(), "Status".to_string()]),
        ..QueryOptions::default()
    };

    let result = match read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 1,
            max_entities: 10,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("exact-match read should reconcile and find the target"),
    };

    assert_eq!(
        result.entities.len(),
        1,
        "the durable-but-unprojected target must be returned, not an empty page"
    );
    assert_eq!(result.entities[0]["Id"].as_str(), Some("ord-target"));
    assert_eq!(
        result.telemetry.fallback_reason,
        QueryPlaneFallbackReason::ProjectionLagReconcile
    );
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::ReadSourceCursor
    );

    let _ = std::fs::remove_file(db_path);
}

/// ARN-89: when there is no matching entity at all, the reconcile must still
/// terminate within the bounded scan budget — no unbounded scan, no error — and
/// report the reconcile reason. This guards against the fallback turning a
/// genuine "not found" into a runaway scan.
#[tokio::test]
async fn exact_match_absent_file_stays_bounded() {
    let db_path =
        std::env::temp_dir().join(format!("temper-query-plane-exact-absent-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("query-plane-exact-absent");

    // One projected Order exists, but NOT the one we ask for.
    let writer = build_order_state_with_store("query-plane-exact-absent-writer", &store);
    writer
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "ord-present",
            "Create",
            serde_json::json!({}),
            &agent_ctx,
        )
        .await
        .expect("create order");
    upsert_order_projection(
        &store,
        &tenant,
        "ord-present",
        serde_json::json!({ "Status": "Draft" }),
        1,
    )
    .await;

    let mut state = build_order_state("query-plane-exact-absent-reader");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    assert!(
        state
            .ensure_entity_loaded(&tenant, "Order", "ord-present")
            .await,
        "ord-present should hydrate from the durable store"
    );
    assert_eq!(
        state.list_entity_ids(&tenant, "Order"),
        vec!["ord-present".to_string()],
        "precondition: index holds the projected Order, the queried id is absent everywhere"
    );

    let budget = QueryPlaneReadBudget {
        default_page_size: 1,
        max_entities: 10,
    };
    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(id_eq_filter("ord-missing")),
        top: Some(1),
        select: Some(vec!["Id".to_string(), "Status".to_string()]),
        ..QueryOptions::default()
    };

    let result = match read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget,
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("absent exact-match read should return cleanly, not error"),
    };

    assert!(
        result.entities.is_empty(),
        "no entity matches the filter, so the result must be empty"
    );
    assert_eq!(
        result.telemetry.fallback_reason,
        QueryPlaneFallbackReason::ProjectionLagReconcile
    );
    assert!(
        result.telemetry.candidate_count <= budget.scan_candidate_budget(),
        "reconcile must stay within the bounded scan budget (got {}, budget {})",
        result.telemetry.candidate_count,
        budget.scan_candidate_budget()
    );

    let _ = std::fs::remove_file(db_path);
}

/// Hot path is unaffected: a fully-projected, index-current exact-match read is
/// served by native pushdown with no reconcile and no authoritative scan. This
/// proves the ARN-89 fix only fires under projection lag, not on every read.
#[tokio::test]
async fn populated_current_projection_uses_native_no_reconcile() {
    let db_path =
        std::env::temp_dir().join(format!("temper-query-plane-native-hot-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let tenant = TenantId::default();
    let agent_ctx = AgentContext::for_service("query-plane-native-hot");

    // Two Orders, both indexed AND projected: the index is current and the
    // native page can answer the query directly.
    let mut state = build_order_state("query-plane-native-hot");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    for entity_id in ["ord-aa", "ord-bb"] {
        state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                entity_id,
                "Create",
                serde_json::json!({}),
                &agent_ctx,
            )
            .await
            .expect("create order");
        upsert_order_projection(
            &store,
            &tenant,
            entity_id,
            serde_json::json!({ "Status": "Draft" }),
            1,
        )
        .await;
    }

    let mut ids = state.list_entity_ids(&tenant, "Order");
    ids.sort();
    assert_eq!(
        ids,
        vec!["ord-aa".to_string(), "ord-bb".to_string()],
        "precondition: both Orders are in the in-memory index"
    );

    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(id_eq_filter("ord-aa")),
        top: Some(1),
        select: Some(vec!["Id".to_string(), "Status".to_string()]),
        ..QueryOptions::default()
    };

    let result = match read_entity_set_from_query_plane(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 1,
            max_entities: 10,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("native pushdown read should succeed on the hot path"),
    };

    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["Id"].as_str(), Some("ord-aa"));
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::NativePagePushdown,
        "a current projection must be answered natively, not via an authoritative scan"
    );
    assert_eq!(
        result.telemetry.fallback_reason,
        QueryPlaneFallbackReason::None,
        "no reconcile should fire when the projection is current"
    );

    let _ = std::fs::remove_file(db_path);
}
