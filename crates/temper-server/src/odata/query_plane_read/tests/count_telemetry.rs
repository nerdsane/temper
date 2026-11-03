//! The count pass contributes scan work, not page metadata or returned rows.

use super::*;
use temper_odata::query::types::parse_query_options;
use temper_runtime::scheduler::install_deterministic_context;
use temper_store_sim::SimEventStore;

async fn seed_orders(state: &ServerState) {
    let tenant = TenantId::default();
    for id in (0..12)
        .map(|index| format!("cnt-{index:02}"))
        .chain((0..3).map(|index| format!("other-{index}")))
    {
        let response = state
            .get_or_create_tenant_entity(&tenant, "Order", &id, serde_json::json!({ "Id": id }))
            .await
            .expect("create telemetry fixture");
        assert!(response.success);
    }
}

fn request<'a>(
    state: &'a ServerState,
    tenant: &'a TenantId,
    security_ctx: &'a SecurityContext,
    options: &'a QueryOptions,
) -> QueryPlaneReadRequest<'a> {
    QueryPlaneReadRequest {
        state,
        tenant,
        security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: options,
        budget: QueryPlaneReadBudget {
            default_page_size: 5,
            max_entities: 100,
        },
    }
}

#[tokio::test]
async fn sim_count_telemetry_includes_both_scans_without_double_counting_page_one() {
    for seed in 0..16 {
        let (_guard, _clock, _ids) = install_deterministic_context(seed);
        let mut state = build_order_state("count-telemetry-sim");
        state.set_storage_stack(StorageStack::from_sim(SimEventStore::no_faults(seed), None));
        seed_orders(&state).await;
        let tenant = TenantId::default();
        let security_ctx = SecurityContext::system();
        let mut options = parse_query_options(
            "$filter=startswith(Id,'cnt-')&$orderby=Id&$top=5&$count=true&$select=Id",
        )
        .unwrap();
        let mut telemetry = Vec::new();
        for (page, size) in [5, 5, 2].into_iter().enumerate() {
            let result =
                read_entity_set_from_query_plane(request(&state, &tenant, &security_ctx, &options))
                    .await
                    .unwrap_or_else(|_| panic!("seed {seed}, page {page}: read failed"));
            assert_eq!(result.count, Some(12));
            assert_eq!(result.entities.len(), size, "lookahead must not leak");
            assert_eq!(result.next_skiptoken.is_some(), page < 2);
            options.skiptoken = result.next_skiptoken;
            telemetry.push(result.telemetry);
        }
        assert_eq!(
            telemetry
                .iter()
                .map(|t| t.candidate_count)
                .collect::<Vec<_>>(),
            [15, 30, 30],
            "seed {seed}: continuation must expose both full-proof scans"
        );
        assert_eq!(
            telemetry
                .iter()
                .map(|t| t.materialized_count)
                .collect::<Vec<_>>(),
            [15, 30, 30]
        );
        assert_eq!(
            telemetry
                .iter()
                .map(|t| t.returned_count)
                .collect::<Vec<_>>(),
            [5, 5, 2],
            "returned rows exclude count work and lookahead"
        );
        for page in telemetry {
            assert_eq!(page.strategy, QueryPlaneReadStrategy::ReadSourceCursor);
            assert_eq!(page.select_count, 1);
            assert!(page.select_requested);
            assert!(!page.catalog_select_projection);
        }
    }
}

#[tokio::test]
async fn native_count_telemetry_preserves_source_page_metadata() {
    let dir = tempfile::tempdir().expect("isolated telemetry database");
    let url = format!("file:{}", dir.path().join("count.db").display());
    let store = TursoEventStore::new(&url, None).await.unwrap();
    let mut state = build_order_state("count-telemetry-turso");
    state.set_storage_stack(StorageStack::from_turso(store));
    seed_orders(&state).await;
    let tenant = TenantId::default();
    let security_ctx = SecurityContext::system();
    // Nullable Notes ordering requires a source-cursor page, while counting can
    // use native pages. The count pass must not replace the page's strategy.
    let mut options = parse_query_options(
        "$filter=startswith(Id,'cnt-')&$orderby=Notes&$top=5&$count=true&$select=Id",
    )
    .unwrap();
    let first = read_entity_set_from_query_plane(request(&state, &tenant, &security_ctx, &options))
        .await
        .unwrap_or_else(|_| panic!("first page read"));
    assert_eq!(first.telemetry.candidate_count, 15);
    options.skiptoken = Some(first.next_skiptoken.expect("continuation"));

    let count_options = QueryOptions {
        filter: options.filter.clone(),
        count: Some(true),
        top: Some(0),
        ..QueryOptions::default()
    };
    let count = read_entity_set_page(request(&state, &tenant, &security_ctx, &count_options))
        .await
        .unwrap_or_else(|_| panic!("reference count read"));
    assert_eq!(
        count.telemetry.strategy,
        QueryPlaneReadStrategy::NativePagePushdown
    );
    assert!(count.telemetry.pushdown_sparse_probe_count > 0);
    let page_options = QueryOptions {
        count: None,
        ..options.clone()
    };
    let page =
        read_entity_set_from_query_plane(request(&state, &tenant, &security_ctx, &page_options))
            .await
            .unwrap_or_else(|_| panic!("reference page read"));
    assert_eq!(
        page.telemetry.strategy,
        QueryPlaneReadStrategy::ReadSourceCursor
    );

    let result =
        read_entity_set_from_query_plane(request(&state, &tenant, &security_ctx, &options))
            .await
            .unwrap_or_else(|_| panic!("counted continuation read"));
    assert_eq!(result.entities, page.entities);
    assert_eq!(result.count, count.count);
    let mut expected = page.telemetry;
    expected.candidate_count += count.telemetry.candidate_count;
    expected.materialized_count += count.telemetry.materialized_count;
    expected.catalog_shadow_check_budget += count.telemetry.catalog_shadow_check_budget;
    expected.catalog_shadow_check_scheduled += count.telemetry.catalog_shadow_check_scheduled;
    expected.pushdown_sparse_probe_count += count.telemetry.pushdown_sparse_probe_count;
    expected.pushdown_page_count += count.telemetry.pushdown_page_count;
    assert_eq!(
        result.telemetry, expected,
        "only additive scan counters change"
    );
    assert_eq!(first.telemetry.returned_count, first.entities.len());
    assert_eq!(result.telemetry.returned_count, result.entities.len());
    assert_eq!(result.entities.len(), 5);
}

#[test]
fn adding_count_scan_work_preserves_page_metadata() {
    use super::super::types::QueryPlaneReadTelemetry;

    let mut page = QueryPlaneReadTelemetry {
        strategy: QueryPlaneReadStrategy::ReadSourceCursor,
        fallback_reason: QueryPlaneFallbackReason::ProjectionLagReconcile,
        filter_pushdown: false,
        catalog_materialization: false,
        candidate_count: 7,
        materialized_count: 5,
        returned_count: 5,
        catalog_shadow_check_budget: 2,
        catalog_shadow_check_scheduled: 1,
        coverage: QueryPlaneCoverageReport {
            missing: 2,
            matched: 1,
        },
        select_requested: true,
        catalog_select_projection: true,
        select_count: 1,
        pushdown_sparse_page: false,
        pushdown_sparse_probe_count: 2,
        pushdown_page_count: 7,
    };
    let count = QueryPlaneReadTelemetry {
        strategy: QueryPlaneReadStrategy::NativePagePushdown,
        fallback_reason: QueryPlaneFallbackReason::None,
        filter_pushdown: true,
        catalog_materialization: true,
        candidate_count: 11,
        materialized_count: 9,
        returned_count: 0,
        catalog_shadow_check_budget: 3,
        catalog_shadow_check_scheduled: 2,
        coverage: QueryPlaneCoverageReport {
            missing: 6,
            matched: 4,
        },
        select_requested: false,
        catalog_select_projection: false,
        select_count: 0,
        pushdown_sparse_page: true,
        pushdown_sparse_probe_count: 4,
        pushdown_page_count: 11,
    };
    let expected = QueryPlaneReadTelemetry {
        candidate_count: 18,
        materialized_count: 14,
        catalog_shadow_check_budget: 5,
        catalog_shadow_check_scheduled: 3,
        pushdown_sparse_probe_count: 6,
        pushdown_page_count: 18,
        ..page.clone()
    };
    page.add_scan_work(&count);
    assert_eq!(page, expected);
}
