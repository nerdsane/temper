use super::*;

async fn upsert_order_projection_with_status(
    store: &TursoEventStore,
    tenant: &TenantId,
    entity_id: &str,
    status: &str,
    mut fields: serde_json::Value,
    sequence_nr: u64,
) {
    fields["Id"] = serde_json::json!(entity_id);
    let state_json = serde_json::json!({
        "entity_type": "Order",
        "entity_id": entity_id,
        "status": status,
        "fields": fields,
        "sequence_nr": sequence_nr,
        "events": [],
    });
    store
        .upsert_query_projection_with_state(
            tenant.as_str(),
            "Order",
            entity_id,
            status,
            state_json.get("fields").unwrap(),
            &state_json,
            sequence_nr,
        )
        .await
        .expect("upsert projection");
}

#[test]
fn native_plan_pushes_file_lookup_equalities_with_status_ne_recheck() {
    let state = build_order_state("query-plane-native-file-status-ne");
    let tenant = TenantId::default();
    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::BinaryOp {
                    left: Box::new(FilterExpr::Property("Path".to_string())),
                    op: BinaryOperator::Eq,
                    right: Box::new(FilterExpr::Literal(ODataValue::String(
                        "/proofs/live.txt".to_string(),
                    ))),
                }),
                op: BinaryOperator::And,
                right: Box::new(FilterExpr::BinaryOp {
                    left: Box::new(FilterExpr::Property("WorkspaceId".to_string())),
                    op: BinaryOperator::Eq,
                    right: Box::new(FilterExpr::Literal(ODataValue::String("ws-1".to_string()))),
                }),
            }),
            op: BinaryOperator::And,
            right: Box::new(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::Property("Status".to_string())),
                op: BinaryOperator::Ne,
                right: Box::new(FilterExpr::Literal(ODataValue::String(
                    "Archived".to_string(),
                ))),
            }),
        }),
        top: Some(1),
        ..QueryOptions::default()
    };
    let request = QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security_ctx,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &query_options,
        budget: QueryPlaneReadBudget {
            default_page_size: 100,
            max_entities: 100,
        },
    };

    let plan = native_candidate_page_plan(&request).expect("native plan");

    assert!(plan.filter_pushdown);
    assert!(plan.where_clause.contains("AND"));
    assert!(!plan.where_clause.contains("status !="));
    assert_eq!(
        plan.params,
        vec!["Path", "/proofs/live.txt", "WorkspaceId", "ws-1"]
    );
    assert!(should_try_native_before_catalog_coverage(&request, &plan));
}

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
async fn context_prep_shaped_filter_with_huge_top_uses_bounded_native_page() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-query-plane-session-filter-{}.db",
        sim_uuid()
    ));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = build_order_state("query-plane-session-filter");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let tenant = TenantId::default();

    for index in 0usize..1200 {
        upsert_order_projection(
            &store,
            &tenant,
            &format!("entry-{index:04}"),
            serde_json::json!({
                "SessionId": "session-hot",
                "ParentEntryId": format!("entry-{:04}", index.saturating_sub(1)),
            }),
            index as u64 + 1,
        )
        .await;
    }
    upsert_order_projection(
        &store,
        &tenant,
        "entry-other",
        serde_json::json!({ "SessionId": "session-cold" }),
        2000,
    )
    .await;

    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("SessionId".to_string())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(
                "session-hot".to_string(),
            ))),
        }),
        top: Some(10_000),
        select: Some(vec!["Id".to_string(), "SessionId".to_string()]),
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
            default_page_size: 100,
            max_entities: 1000,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("context-prep shaped filtered read should use native page pushdown"),
    };

    assert_eq!(result.entities.len(), 1000);
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::NativePagePushdown
    );
    assert!(result.telemetry.filter_pushdown);
    assert_eq!(result.telemetry.candidate_count, 1000);
    assert_eq!(result.telemetry.materialized_count, 1000);
    assert_eq!(result.telemetry.pushdown_page_count, 1000);
    assert_eq!(result.telemetry.returned_count, 1000);

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn file_point_lookup_with_status_ne_uses_lossless_equality_candidates() {
    let db_path = std::env::temp_dir().join(format!(
        "temper-query-plane-file-status-ne-{}.db",
        sim_uuid()
    ));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = build_order_state("query-plane-file-status-ne");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let tenant = TenantId::default();
    let target_path = "/proofs/katagami-temper-write-live.txt";
    let workspace_id = "ws-proof";

    for index in 0usize..120 {
        upsert_order_projection(
            &store,
            &tenant,
            &format!("noise-{index:04}"),
            serde_json::json!({
                "Path": format!("/noise/{index:04}.txt"),
                "WorkspaceId": workspace_id,
            }),
            index as u64 + 1,
        )
        .await;
    }
    upsert_order_projection_with_status(
        &store,
        &tenant,
        "zz-target-archived",
        "Archived",
        serde_json::json!({
            "Path": target_path,
            "WorkspaceId": workspace_id,
        }),
        2000,
    )
    .await;
    upsert_order_projection_with_status(
        &store,
        &tenant,
        "zz-target-ready",
        "Created",
        serde_json::json!({
            "Path": target_path,
            "WorkspaceId": workspace_id,
        }),
        2001,
    )
    .await;

    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::BinaryOp {
                    left: Box::new(FilterExpr::Property("Path".to_string())),
                    op: BinaryOperator::Eq,
                    right: Box::new(FilterExpr::Literal(ODataValue::String(
                        target_path.to_string(),
                    ))),
                }),
                op: BinaryOperator::And,
                right: Box::new(FilterExpr::BinaryOp {
                    left: Box::new(FilterExpr::Property("WorkspaceId".to_string())),
                    op: BinaryOperator::Eq,
                    right: Box::new(FilterExpr::Literal(ODataValue::String(
                        workspace_id.to_string(),
                    ))),
                }),
            }),
            op: BinaryOperator::And,
            right: Box::new(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::Property("Status".to_string())),
                op: BinaryOperator::Ne,
                right: Box::new(FilterExpr::Literal(ODataValue::String(
                    "Archived".to_string(),
                ))),
            }),
        }),
        top: Some(1),
        select: Some(vec![
            "Id".to_string(),
            "Path".to_string(),
            "WorkspaceId".to_string(),
        ]),
        ..QueryOptions::default()
    };

    let request = QueryPlaneReadRequest {
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
    };
    let native_plan = native_candidate_page_plan(&request).expect("native plan");
    assert!(native_plan.filter_pushdown);
    let (candidate_ids, _) = store
        .query_field_index_page(
            tenant.as_str(),
            "Order",
            &native_plan.where_clause,
            native_plan.params.clone(),
            &[],
            0,
            10,
            false,
        )
        .await
        .expect("native candidate query should execute");
    assert_eq!(
        candidate_ids,
        vec![
            "zz-target-archived".to_string(),
            "zz-target-ready".to_string()
        ]
    );

    let result = match read_entity_set_from_query_plane(request).await {
        Ok(result) => result,
        Err(QueryPlaneReadError::QueryTooLarge { telemetry }) => panic!(
            "file point lookup should push down equality candidates: QueryTooLarge filter_pushdown={} fallback={:?} candidates={} pushed={}",
            telemetry.filter_pushdown,
            telemetry.fallback_reason,
            telemetry.candidate_count,
            telemetry.pushdown_page_count
        ),
        Err(QueryPlaneReadError::AuthorizationDenied(_)) => {
            panic!("file point lookup should not be denied by read authorization")
        }
    };

    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["Id"].as_str(), Some("zz-target-ready"));
    assert_eq!(result.entities[0]["Path"].as_str(), Some(target_path));
    assert_eq!(
        result.entities[0]["WorkspaceId"].as_str(),
        Some(workspace_id)
    );
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::NativePagePushdown
    );
    assert!(result.telemetry.filter_pushdown);
    assert_eq!(result.telemetry.candidate_count, 2);
    assert_eq!(result.telemetry.materialized_count, 2);
    assert_eq!(result.telemetry.pushdown_page_count, 2);
    assert_eq!(result.telemetry.returned_count, 1);

    let _ = std::fs::remove_file(db_path);
}

#[tokio::test]
async fn session_entry_chain_parent_lookup_uses_bounded_native_page() {
    let db_dir = tempfile::tempdir().expect("create isolated query-plane db dir");
    let db_path = db_dir.path().join("session-parent.db");
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = build_order_state("query-plane-session-parent");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let tenant = TenantId::default();

    for index in 0usize..1200 {
        upsert_order_projection(
            &store,
            &tenant,
            &format!("entry-{index:04}"),
            serde_json::json!({
                "SessionId": "session-hot",
                "ParentEntryId": format!("entry-{:04}", index.saturating_sub(1)),
            }),
            index as u64 + 1,
        )
        .await;
    }

    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("ParentEntryId".to_string())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(
                "entry-1198".to_string(),
            ))),
        }),
        top: Some(1),
        select: Some(vec![
            "Id".to_string(),
            "SessionId".to_string(),
            "ParentEntryId".to_string(),
        ]),
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
            default_page_size: 100,
            max_entities: 1000,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("SessionEntry parent lookup should use native page pushdown"),
    };

    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["Id"].as_str(), Some("entry-1199"));
    assert_eq!(
        result.entities[0]["ParentEntryId"].as_str(),
        Some("entry-1198")
    );
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::NativePagePushdown
    );
    assert!(result.telemetry.filter_pushdown);
    assert_eq!(result.telemetry.candidate_count, 1);
    assert_eq!(result.telemetry.pushdown_page_count, 1);
}

#[tokio::test]
async fn session_entry_leaf_id_lookup_uses_bounded_native_page() {
    let db_dir = tempfile::tempdir().expect("create isolated query-plane db dir");
    let db_path = db_dir.path().join("session-leaf.db");
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let mut state = build_order_state("query-plane-session-leaf");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let tenant = TenantId::default();

    for index in 0usize..1200 {
        upsert_order_projection(
            &store,
            &tenant,
            &format!("entry-{index:04}"),
            serde_json::json!({
                "SessionId": "session-hot",
                "ParentEntryId": format!("entry-{:04}", index.saturating_sub(1)),
            }),
            index as u64 + 1,
        )
        .await;
    }

    let security_ctx = SecurityContext::system();
    let query_options = QueryOptions {
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Id".to_string())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(
                "entry-1199".to_string(),
            ))),
        }),
        top: Some(1),
        select: Some(vec![
            "Id".to_string(),
            "SessionId".to_string(),
            "ParentEntryId".to_string(),
        ]),
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
            default_page_size: 100,
            max_entities: 1000,
        },
    })
    .await
    {
        Ok(result) => result,
        Err(_) => panic!("SessionEntry leaf id lookup should use native page pushdown"),
    };

    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["Id"].as_str(), Some("entry-1199"));
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::NativePagePushdown
    );
    assert!(result.telemetry.filter_pushdown);
    assert_eq!(result.telemetry.candidate_count, 1);
    assert_eq!(result.telemetry.pushdown_page_count, 1);
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
