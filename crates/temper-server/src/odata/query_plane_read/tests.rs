use super::types::{
    QueryPlaneFallbackReason, QueryPlaneReadBudget, QueryPlaneReadError, QueryPlaneReadRequest,
    QueryPlaneReadStrategy,
};
use super::*;
use crate::registry::SpecRegistry;
use crate::request_context::AgentContext;
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
