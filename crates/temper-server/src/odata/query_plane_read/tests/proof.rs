use super::*;

#[tokio::test]
async fn ordinary_null_filter_uses_lossless_candidate_scan() {
    let db_path =
        std::env::temp_dir().join(format!("temper-query-plane-null-proof-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = build_order_state("query-plane-null-proof");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let tenant = TenantId::default();

    upsert_order_projection(
        &store,
        &tenant,
        "ord-null-notes",
        serde_json::json!({ "Notes": null }),
        1,
    )
    .await;
    upsert_order_projection(
        &store,
        &tenant,
        "ord-missing-notes",
        serde_json::json!({}),
        2,
    )
    .await;
    upsert_order_projection(
        &store,
        &tenant,
        "ord-text-notes",
        serde_json::json!({ "Notes": "packed" }),
        3,
    )
    .await;

    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Notes".to_string())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::Null)),
        }),
        count: Some(true),
        select: Some(vec!["Id".to_string(), "Notes".to_string()]),
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
            default_page_size: 10,
            max_entities: 10,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("null filter proof should succeed"),
    };

    assert_eq!(result.count, Some(1));
    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["Id"].as_str(), Some("ord-null-notes"));
    assert!(result.entities[0]["Notes"].is_null());
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::NativePagePushdown
    );
    assert!(!result.telemetry.filter_pushdown);
    assert_eq!(result.telemetry.candidate_count, 3);

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn nullable_scalar_order_uses_read_source_full_proof() {
    let db_path =
        std::env::temp_dir().join(format!("temper-query-plane-null-order-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = build_order_state("query-plane-null-order");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let tenant = TenantId::default();

    upsert_order_projection(
        &store,
        &tenant,
        "ord-text-notes",
        serde_json::json!({ "Notes": "packed" }),
        1,
    )
    .await;
    upsert_order_projection(
        &store,
        &tenant,
        "ord-null-notes",
        serde_json::json!({ "Notes": null }),
        2,
    )
    .await;

    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        orderby: Some(vec![OrderByClause {
            property: "Notes".to_string(),
            direction: OrderDirection::Asc,
        }]),
        top: Some(1),
        select: Some(vec!["Id".to_string(), "Notes".to_string()]),
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
            default_page_size: 10,
            max_entities: 10,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("read-source order proof should succeed"),
    };

    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["Id"].as_str(), Some("ord-null-notes"));
    assert!(result.entities[0]["Notes"].is_null());
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::ReadSourceCursor
    );
    assert_eq!(
        result.telemetry.fallback_reason,
        QueryPlaneFallbackReason::NativePageUnavailable
    );

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn unsafe_order_uses_read_source_full_proof() {
    let db_path =
        std::env::temp_dir().join(format!("temper-query-plane-order-proof-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = build_order_state("query-plane-order-proof");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let tenant = TenantId::default();

    upsert_order_projection(
        &store,
        &tenant,
        "ord-with-score",
        serde_json::json!({ "AdHocScore": 10 }),
        1,
    )
    .await;
    upsert_order_projection(
        &store,
        &tenant,
        "ord-missing-score",
        serde_json::json!({}),
        2,
    )
    .await;

    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        orderby: Some(vec![OrderByClause {
            property: "AdHocScore".to_string(),
            direction: OrderDirection::Desc,
        }]),
        top: Some(1),
        select: Some(vec!["Id".to_string()]),
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
            default_page_size: 10,
            max_entities: 10,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("read-source proof should succeed"),
    };

    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["Id"].as_str(), Some("ord-missing-score"));
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::ReadSourceCursor
    );
    assert_eq!(
        result.telemetry.fallback_reason,
        QueryPlaneFallbackReason::NativePageUnavailable
    );

    let _ = std::fs::remove_file(db_path);
}
