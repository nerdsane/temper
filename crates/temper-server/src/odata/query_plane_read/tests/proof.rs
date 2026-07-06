use super::*;

/// ADR-0153 read-plane proof on real Postgres (the prod engine): a `$filter` that
/// is exactly the declared `[[key]]` resolves to the single matching entity via
/// `entity_key_index` — a bounded candidate (no full-type scan → the budget that
/// raises the 413 can never trip). Gated on DATABASE_URL; unique tenant; cleans up.
///
/// Proves `keyed_candidate_ids` composes the verified pieces against a live DB:
/// real `append_with_keys` co-commit → real `lookup_by_key` → bounded `[id]`.
#[test]
fn keyed_filter_resolves_to_bounded_candidate_on_postgres() {
    use temper_runtime::persistence::{
        EntityKeyRow, EventMetadata, EventStore, PersistenceEnvelope,
    };
    use temper_runtime::scheduler::{sim_now, sim_uuid as runtime_sim_uuid};

    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) => return,
    };

    sqlx::test_block_on(async {
        let pool = sqlx::PgPool::connect(&database_url).await.unwrap();
        temper_store_postgres::migration::run_migrations(&pool)
            .await
            .unwrap();
        let store = temper_store_postgres::PostgresEventStore::new(pool.clone());
        let mut state = build_order_state("query-plane-keyed-pg");
        state.set_storage_stack(StorageStack::from_postgres(store.clone()));
        // from_registry leaves transition_tables empty; the keyed fast path reads
        // the declared `[[key]]` from the Order table, so install it (with the
        // ws_path key from order.ioa.toml).
        state.transition_tables = std::sync::Arc::new(
            [(
                "Order".to_string(),
                std::sync::Arc::new(temper_jit::table::TransitionTable::from_ioa_source(
                    ORDER_IOA,
                )),
            )]
            .into_iter()
            .collect(),
        );

        // Unique tenant isolates this run on the shared DB. The Order table (with
        // the declared `ws_path` key) is found by entity_type regardless of tenant.
        let tenant_str = format!("tenant-keyed-pg-{}", runtime_sim_uuid());
        let tenant = TenantId::from(tenant_str.clone());
        let workspace_id = "ws-keyed";
        let target_path = "/proofs/keyed-read.txt";
        let target_id = "ord-keyed-target";

        // Write the target's key row via the REAL co-commit (journal + entity_key_index).
        let key_hash = crate::key_index::canonical_key_hash(
            "ws_path",
            &["WorkspaceId".to_string(), "Path".to_string()],
            &serde_json::json!({ "WorkspaceId": workspace_id, "Path": target_path })
                .as_object()
                .unwrap()
                .clone(),
        )
        .expect("complete key");
        let envelope = PersistenceEnvelope {
            sequence_nr: 1,
            event_type: "Create".to_string(),
            payload: serde_json::json!({ "WorkspaceId": workspace_id, "Path": target_path }),
            metadata: EventMetadata {
                event_id: runtime_sim_uuid(),
                causation_id: runtime_sim_uuid(),
                correlation_id: runtime_sim_uuid(),
                timestamp: sim_now(),
                actor_id: "keyed-pg-proof".to_string(),
            },
        };
        store
            .append_with_keys(
                &format!("{tenant_str}:Order:{target_id}"),
                0,
                &[envelope],
                &[EntityKeyRow {
                    key_name: "ws_path".to_string(),
                    key_hash,
                }],
            )
            .await
            .expect("co-commit key row");

        // AMPLIFICATION BOUND (ADR-0148 not regressed): the synchronous co-commit
        // wrote exactly K=1 key row and ZERO broad-index rows. The S-wide
        // entity_field_index (the write we migrated to async) is NOT touched by the
        // append path — it is still written only by the async projection. This is
        // the empirical proof the fix does not bring back synchronous write
        // amplification.
        let key_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM entity_key_index WHERE tenant = $1 AND entity_id = $2",
        )
        .bind(&tenant_str)
        .bind(target_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(key_rows, 1, "co-commit writes exactly K=1 declared-key row");
        let broad_index_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM entity_field_index WHERE tenant = $1 AND entity_id = $2",
        )
        .bind(&tenant_str)
        .bind(target_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            broad_index_rows, 0,
            "the synchronous append must NOT write the S-wide broad index (stays async — no amplification regression)"
        );

        let security_ctx = SecurityContext::system();
        // PRESENT: $filter == the declared key -> bounded [target_id].
        let keyed = QueryOptions {
            filter: Some(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::BinaryOp {
                    left: Box::new(FilterExpr::Property("WorkspaceId".to_string())),
                    op: BinaryOperator::Eq,
                    right: Box::new(FilterExpr::Literal(ODataValue::String(
                        workspace_id.to_string(),
                    ))),
                }),
                op: BinaryOperator::And,
                right: Box::new(FilterExpr::BinaryOp {
                    left: Box::new(FilterExpr::Property("Path".to_string())),
                    op: BinaryOperator::Eq,
                    right: Box::new(FilterExpr::Literal(ODataValue::String(
                        target_path.to_string(),
                    ))),
                }),
            }),
            ..QueryOptions::default()
        };
        let request = QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security_ctx,
            entity_type: "Order",
            entity_set_name: "Orders",
            query_options: &keyed,
            budget: QueryPlaneReadBudget {
                default_page_size: 10,
                max_entities: 10,
            },
        };
        assert_eq!(
            keyed_candidate_ids(&request).await,
            Some(vec![target_id.to_string()]),
            "a $filter matching the declared key must resolve to the bounded single id via entity_key_index"
        );

        // CONTROL: a non-key filter does not resolve -> None (falls back to scan).
        let non_key = QueryOptions {
            filter: Some(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::Property("Status".to_string())),
                op: BinaryOperator::Eq,
                right: Box::new(FilterExpr::Literal(ODataValue::String(
                    "Created".to_string(),
                ))),
            }),
            ..QueryOptions::default()
        };
        let control = QueryPlaneReadRequest {
            query_options: &non_key,
            ..request
        };
        assert_eq!(
            keyed_candidate_ids(&control).await,
            None,
            "a non-key filter must decline the keyed fast path"
        );

        // ABSENT key: a declared-key filter for a key NO entity holds resolves to a
        // keyed MISS. Pre-backfill that returns None (fall back to scan), NOT an
        // authoritative empty — a missing key row may be a not-yet-backfilled entity.
        let absent = QueryOptions {
            filter: Some(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::BinaryOp {
                    left: Box::new(FilterExpr::Property("WorkspaceId".to_string())),
                    op: BinaryOperator::Eq,
                    right: Box::new(FilterExpr::Literal(ODataValue::String(workspace_id.into()))),
                }),
                op: BinaryOperator::And,
                right: Box::new(FilterExpr::BinaryOp {
                    left: Box::new(FilterExpr::Property("Path".to_string())),
                    op: BinaryOperator::Eq,
                    right: Box::new(FilterExpr::Literal(ODataValue::String(
                        "/proofs/does-not-exist.txt".to_string(),
                    ))),
                }),
            }),
            ..QueryOptions::default()
        };
        let absent_req = QueryPlaneReadRequest {
            query_options: &absent,
            ..request
        };
        assert_eq!(
            keyed_candidate_ids(&absent_req).await,
            None,
            "a keyed miss must decline to scan fallback pre-backfill (not authoritative-empty)"
        );

        // Clean up this run's rows.
        let _ = sqlx::query("DELETE FROM entity_key_index WHERE tenant = $1")
            .bind(&tenant_str)
            .execute(&pool)
            .await;
        let _ = sqlx::query("DELETE FROM events WHERE tenant = $1")
            .bind(&tenant_str)
            .execute(&pool)
            .await;
    });
}

/// ADR-0153 boundary: the keyed fast path engages ONLY for a pure-equality filter
/// that is exactly a declared `[[key]]`. Every other shape must decline (return
/// None) so the read falls back to the existing scan/pushdown path — no behavior
/// change, no false hit. No DB needed: all these decline before any store call.
#[tokio::test]
async fn keyed_fast_path_declines_non_key_shapes() {
    let mut state = build_order_state("query-plane-keyed-decline");
    state.transition_tables = std::sync::Arc::new(
        [(
            "Order".to_string(),
            std::sync::Arc::new(temper_jit::table::TransitionTable::from_ioa_source(
                ORDER_IOA,
            )),
        )]
        .into_iter()
        .collect(),
    );
    let tenant = TenantId::default();
    let security_ctx = SecurityContext::system();
    let budget = QueryPlaneReadBudget {
        default_page_size: 10,
        max_entities: 10,
    };

    let eq = |prop: &str, val: &str| FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property(prop.to_string())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String(val.to_string()))),
    };
    let and = |l: FilterExpr, r: FilterExpr| FilterExpr::BinaryOp {
        left: Box::new(l),
        op: BinaryOperator::And,
        right: Box::new(r),
    };

    // The declared key is (WorkspaceId, Path). Each of these must decline:
    let cases: Vec<(&str, QueryOptions)> = vec![
        // non-lossless conjunct (the agents' original Status ne failure shape)
        (
            "status_ne",
            QueryOptions {
                filter: Some(and(
                    and(eq("WorkspaceId", "ws"), eq("Path", "/a")),
                    FilterExpr::BinaryOp {
                        left: Box::new(FilterExpr::Property("Status".to_string())),
                        op: BinaryOperator::Ne,
                        right: Box::new(FilterExpr::Literal(ODataValue::String("Archived".into()))),
                    },
                )),
                ..QueryOptions::default()
            },
        ),
        // eq null (the Directories root lookup shape)
        (
            "eq_null",
            QueryOptions {
                filter: Some(and(
                    eq("WorkspaceId", "ws"),
                    FilterExpr::BinaryOp {
                        left: Box::new(FilterExpr::Property("ParentId".to_string())),
                        op: BinaryOperator::Eq,
                        right: Box::new(FilterExpr::Literal(ODataValue::Null)),
                    },
                )),
                ..QueryOptions::default()
            },
        ),
        // partial key (only one of the two key properties)
        (
            "partial_key",
            QueryOptions {
                filter: Some(eq("WorkspaceId", "ws")),
                ..QueryOptions::default()
            },
        ),
        // non-key property
        (
            "non_key_prop",
            QueryOptions {
                filter: Some(eq("Notes", "hi")),
                ..QueryOptions::default()
            },
        ),
        // $orderby present (a point read has no ordering to honor)
        (
            "with_orderby",
            QueryOptions {
                filter: Some(and(eq("WorkspaceId", "ws"), eq("Path", "/a"))),
                orderby: Some(vec![OrderByClause {
                    property: "Path".to_string(),
                    direction: OrderDirection::Asc,
                }]),
                ..QueryOptions::default()
            },
        ),
    ];

    for (name, query_options) in &cases {
        let request = QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security_ctx,
            entity_type: "Order",
            entity_set_name: "Orders",
            query_options,
            budget,
        };
        assert_eq!(
            keyed_candidate_ids(&request).await,
            None,
            "keyed fast path must decline shape '{name}' (falls back to scan/pushdown)"
        );
    }
}

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

    // `prop eq null` is relational IS NULL: it matches both an explicit JSON `null`
    // (`ord-null-notes`) AND a row that OMITS the property (`ord-missing-notes`) — a
    // missing schema property IS NULL. (ARN-68: this is the same rule that lets a
    // Directory root, which has no `ParentId` field, match `ParentId eq null`. The
    // text-valued `ord-text-notes` is excluded.)
    assert_eq!(result.count, Some(2));
    assert_eq!(result.entities.len(), 2);
    let ids: std::collections::BTreeSet<&str> = result
        .entities
        .iter()
        .filter_map(|entity| entity["Id"].as_str())
        .collect();
    assert!(
        ids.contains("ord-null-notes"),
        "explicit null matches eq null"
    );
    assert!(
        ids.contains("ord-missing-notes"),
        "a missing property is NULL, so it matches eq null"
    );
    assert!(!ids.contains("ord-text-notes"));
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

/// ARN-68 (the SessionEntries-list flavor) shared harness: `count` journal-durable
/// Orders (dispatched, so they enumerate from the journal) that are ALSO projected with
/// `SessionId: "session-hot"` — the prod shape of a type whose cardinality exceeds the
/// scan budget. Returns the state wired to the shared store.
async fn build_large_projected_type(
    store: &TursoEventStore,
    tenant: &TenantId,
    system_name: &str,
    count: usize,
) -> ServerState {
    let mut state = build_order_state(system_name);
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let agent_ctx = AgentContext::for_service(system_name);
    for index in 0..count {
        let entity_id = format!("entry-{index:04}");
        state
            .dispatch_tenant_action(
                tenant,
                "Order",
                &entity_id,
                "Create",
                serde_json::json!({}),
                &agent_ctx,
            )
            .await
            .expect("create order");
        // Deterministic projected fields (high sequence wins over the async queue).
        upsert_order_projection(
            store,
            tenant,
            &entity_id,
            serde_json::json!({ "SessionId": "session-hot" }),
            1_000 + index as u64,
        )
        .await;
    }
    state
}

fn session_filter_options(session: &str) -> QueryOptions {
    QueryOptions {
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("SessionId".to_string())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(session.to_string()))),
        }),
        top: Some(200),
        ..QueryOptions::default()
    }
}

/// ARN-68: an EMPTY bounded native page for an equality-conjunction list on a type
/// LARGER than the scan budget is served bounded when the projection has no gaps —
/// no full-type scan, no 413. This is the prod session-bootstrap shape:
/// `SessionEntries?$filter=SessionId eq '<new>'` against 95k fully-projected entries.
/// Before the fix this was an unconditional budget rejection.
#[tokio::test]
async fn empty_equality_list_on_large_type_is_authoritative_absent_without_scan() {
    let db_path = std::env::temp_dir().join(format!("temper-arn68-empty-list-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let tenant = TenantId::default();
    // 15 journal-durable + projected entities; scan budget is 10.
    let state = build_large_projected_type(&store, &tenant, "arn68-empty-list", 15).await;

    let security_ctx = SecurityContext::system();
    let query_options = session_filter_options("session-brand-new");
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
        Err(_) => panic!("empty equality list on a fully-projected large type must not 413"),
    };
    assert!(result.entities.is_empty());

    let _ = std::fs::remove_file(db_path);
}

/// ARN-68 + ARN-89 at scale: a journal-durable entity whose projected row is MISSING
/// (never projected here; a crash-lost projection in prod) must be FOUND by an equality list
/// even when the type exceeds the scan budget — the gap reconcile materializes only the
/// unprojected entities. A filter matching neither the projected majority nor the gap
/// stays a bounded empty answer. Before the fix both were budget rejections.
#[tokio::test]
async fn equality_list_on_large_type_repairs_unprojected_match_boundedly() {
    let db_path = std::env::temp_dir().join(format!("temper-arn68-gap-list-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let tenant = TenantId::default();
    let state = build_large_projected_type(&store, &tenant, "arn68-gap-list", 15).await;

    // The lagging entity: journal-durable via DIRECT append — its projection was never
    // enqueued (the crash-lost shape, deterministic: no async worker to race). The
    // snapshot carries the queried SessionId for materialization.
    use temper_runtime::persistence::{EventMetadata, EventStore as _, PersistenceEnvelope};
    use temper_runtime::scheduler::{sim_now, sim_uuid as runtime_sim_uuid};
    store
        .append(
            &format!("{tenant}:Order:entry-lagging"),
            0,
            &[PersistenceEnvelope {
                sequence_nr: 1,
                event_type: "Create".to_string(),
                payload: serde_json::json!({
                    "action": "Create",
                    "params": { "SessionId": "session-brand-new" },
                }),
                metadata: EventMetadata {
                    event_id: runtime_sim_uuid(),
                    causation_id: runtime_sim_uuid(),
                    correlation_id: runtime_sim_uuid(),
                    timestamp: sim_now(),
                    actor_id: "arn68-gap-list".to_string(),
                },
            }],
        )
        .await
        .expect("append lagging order");
    let snapshot = serde_json::json!({
        "entity_type": "Order",
        "entity_id": "entry-lagging",
        "status": "Draft",
        "item_count": 0,
        "fields": { "Id": "entry-lagging", "Status": "Draft", "SessionId": "session-brand-new" },
    });
    store
        .save_snapshot(
            &format!("{tenant}:Order:entry-lagging"),
            1,
            &serde_json::to_vec(&snapshot).unwrap(),
        )
        .await
        .expect("seed snapshot");

    let security_ctx = SecurityContext::system();
    let budget = QueryPlaneReadBudget {
        default_page_size: 1,
        max_entities: 1,
    };

    // The unprojected match is repaired and returned — not a 413, not a wrong empty.
    let query_options = session_filter_options("session-brand-new");
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
        Err(_) => panic!("gap reconcile must repair the unprojected match, not reject"),
    };
    assert_eq!(result.entities.len(), 1, "the unprojected entity is found");
    assert_eq!(
        result.telemetry.fallback_reason,
        QueryPlaneFallbackReason::ProjectionLagReconcile
    );

    // A session matching nothing (projected or gap) stays a bounded empty answer.
    let query_options = session_filter_options("session-nowhere");
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
        Err(_) => panic!("a genuine miss with a bounded gap must not 413"),
    };
    assert!(result.entities.is_empty());

    let _ = std::fs::remove_file(db_path);
}

/// ARN-68 rescue with `$skip`/`$select`: the covered prefix must not be double-skipped
/// (page re-runs with skip folded into top; the cursor applies `$skip` once) and
/// `$select` must not strip the union's `entity_id` key.
#[tokio::test]
async fn equality_list_rescue_honors_skip_and_select() {
    let db_path = std::env::temp_dir().join(format!("temper-arn68-skip-list-{}.db", sim_uuid()));
    let _ = std::fs::remove_file(&db_path);
    let db_url = format!("file:{}", db_path.display());
    let store = TursoEventStore::new(&db_url, None)
        .await
        .expect("create local turso db");
    let tenant = TenantId::default();
    let state = build_large_projected_type(&store, &tenant, "arn68-skip-list", 15).await;

    // Two PROJECTED matches for the queried session (visible only to the page)...
    for (id, seq) in [("paged-c1", 5001u64), ("paged-c2", 5002)] {
        upsert_order_projection(
            &store,
            &tenant,
            id,
            serde_json::json!({ "SessionId": "session-paged" }),
            seq,
        )
        .await;
    }
    // ...and they must also be journal-durable so enumeration sees the type as-is.
    // (The 15 harness entities already push the type over budget.)

    let security_ctx = SecurityContext::system();
    let budget = QueryPlaneReadBudget {
        default_page_size: 1,
        max_entities: 1,
    };
    let mut query_options = session_filter_options("session-paged");
    query_options.top = Some(2);
    query_options.skip = Some(1);
    query_options.select = Some(vec!["Id".to_string()]);

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
        Err(_) => panic!("rescue must serve the skip/select page, not reject"),
    };
    // 2 matches, skip 1 → exactly 1 row survives; double-skip would return 0.
    assert_eq!(
        result.entities.len(),
        1,
        "skip must apply exactly once across the union"
    );

    let _ = std::fs::remove_file(db_path);
}
