//! Indexed equality reads over the query plane the journal writes.
use super::*;
use crate::storage::QueryPlaneStore;

pub(super) fn notes(value: &str) -> QueryOptions {
    QueryOptions {
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Notes".into())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(value.into()))),
        }),
        count: Some(true),
        ..QueryOptions::default()
    }
}

/// Same input and instrument before and after the planner change. Timing is
/// deliberately opt-in.
#[tokio::test]
#[ignore = "local measurement: 3000 durable rows, run with --ignored --nocapture"]
async fn measure_equality_delta_3000_rows() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("measure.db").display()),
        None,
    )
    .await
    .unwrap();
    let tenant = TenantId::default();
    let writer = build_order_state_with_store("measure-writer", &store);
    writer
        .dispatch_tenant_action(
            &tenant,
            "Order",
            "ord-00000",
            "AddItem",
            serde_json::json!({"ProductId": "p", "Quantity": 1, "Notes": "needle"}),
            &AgentContext::for_service("measure"),
        )
        .await
        .unwrap();
    let blob = "x".repeat(16_384);
    for start in (0..3000).step_by(100) {
        let rows: Vec<_> = (start..start+100).map(|i| {
            let id = format!("ord-{i:05}");
            let fields = serde_json::json!({"Id": id, "Notes": if i == 0 {"needle"} else {"noise"}, "Body": blob});
            temper_store_turso::QueryProjectionUpsert {
                entity_type: "Order".into(), entity_id: id, status: "Draft".into(),
                fields: fields.clone(), indexed_fields: fields.clone(),
                state: serde_json::json!({"fields": fields, "status": "Draft"}),
                sequence_nr: if i == 0 {2} else {1}, known_new: i != 0,
            }
        }).collect();
        use temper_runtime::persistence::{
            EventMetadata, EventStore, PersistenceAppend, PersistenceEnvelope,
        };
        let appends: Vec<_> = (start..start + 100)
            .filter(|i| *i != 0)
            .map(|i| PersistenceAppend {
                persistence_id: format!("{tenant}:Order:ord-{i:05}"),
                expected_sequence: 0,
                events: vec![PersistenceEnvelope {
                    sequence_nr: 1,
                    event_type: "Create".into(),
                    payload: serde_json::json!({}),
                    metadata: EventMetadata {
                        event_id: sim_uuid(),
                        causation_id: sim_uuid(),
                        correlation_id: sim_uuid(),
                        timestamp: temper_runtime::scheduler::sim_now(),
                        actor_id: "measure".into(),
                    },
                }],
                state_timer: None,
                projection: None,
            })
            .collect();
        store.append_batch(&appends).await.unwrap();
        store
            .upsert_query_projections(tenant.as_str(), &rows)
            .await
            .unwrap();
    }
    let mut state = build_order_state("measure-equality-delta");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let security = SecurityContext::system();
    for lagging in [false, true] {
        for value in ["absent", "needle"] {
            let mut options = notes(value);
            options.count = None;
            let mut times = Vec::new();
            for _ in 0..7 {
                if lagging {
                    store
                        .remove_query_projection(tenant.as_str(), "Order", "ord-00000")
                        .await
                        .unwrap();
                }
                let began = std::time::Instant::now(); // determinism-ok: opt-in performance measurement
                let result = read_entity_set_page(QueryPlaneReadRequest {
                    state: &state,
                    tenant: &tenant,
                    security_ctx: &security,
                    entity_type: "Order",
                    entity_set_name: "Orders",
                    query_options: &options,
                    budget: QueryPlaneReadBudget {
                        default_page_size: 100,
                        max_entities: 1000,
                    },
                })
                .await
                .unwrap_or_else(|_| panic!("measurement read"));
                times.push(began.elapsed().as_micros());
                assert_eq!(result.entities.len(), usize::from(value == "needle"));
                println!(
                    "lagging={lagging} value={value} us={} candidates={} materialized={}",
                    times.last().unwrap(),
                    result.telemetry.candidate_count,
                    result.telemetry.materialized_count
                );
            }
            times.sort();
            println!("lagging={lagging} value={value} median_us={}", times[3]);
        }
    }
}

#[tokio::test]
async fn equality_delta_preserves_row_auth_order_count_and_select() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("auth.db").display()),
        None,
    )
    .await
    .unwrap();
    let state = build_order_state_with_store("delta-auth", &store);
    let tenant = TenantId::default();
    for id in ["ord-00", "ord-01", "ord-02"] {
        let result = state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                id,
                "AddItem",
                serde_json::json!({"ProductId": "p", "Quantity": 1, "Notes": "needle"}),
                &AgentContext::for_service("delta-auth"),
            )
            .await
            .unwrap();
        assert!(result.success);
    }
    for id in ["ord-00", "ord-01"] {
        store
            .remove_query_projection(tenant.as_str(), "Order", id)
            .await
            .unwrap();
    }
    state
        .authz
        .reload_tenant_policies(
            tenant.as_str(),
            r#"
        permit(principal, action == Action::"list", resource is Order);
        permit(principal, action == Action::"read", resource is Order);
        forbid(principal, action == Action::"read", resource == Order::"ord-01");
    "#,
        )
        .unwrap();
    let mut security = SecurityContext::anonymous();
    security.principal.kind = temper_authz::PrincipalKind::Customer;
    security.principal.id = "reader".into();
    let options = QueryOptions {
        orderby: Some(vec![OrderByClause {
            property: "Id".into(),
            direction: OrderDirection::Desc,
        }]),
        skip: Some(1),
        top: Some(1),
        select: Some(vec!["Id".into()]),
        ..notes("needle")
    };
    let result = read_entity_set_page(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &options,
        budget: QueryPlaneReadBudget {
            default_page_size: 10,
            max_entities: 10,
        },
    })
    .await
    .unwrap_or_else(|_| panic!("authorized list must succeed"));
    assert_eq!(result.count, Some(2), "count only allowed matching rows");
    assert_eq!(result.entities.len(), 1);
    assert_eq!(
        result.entities[0]["Id"], "ord-00",
        "order and skip apply after union and authorization"
    );
    assert!(
        result.entities[0].get("Notes").is_none(),
        "select applies last"
    );

    state
        .authz
        .reload_tenant_policies(tenant.as_str(), "forbid(principal, action, resource);")
        .unwrap();
    assert!(matches!(
        read_entity_set_page(QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security,
            entity_type: "Order",
            entity_set_name: "Orders",
            query_options: &options,
            budget: QueryPlaneReadBudget {
                default_page_size: 10,
                max_entities: 10
            },
        })
        .await,
        Err(QueryPlaneReadError::AuthorizationDenied(_))
    ));
}

#[tokio::test]
async fn equality_delta_declines_a_separately_supplied_journal() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("plane.db").display()),
        None,
    )
    .await
    .unwrap();
    let journal =
        crate::storage::BoxedEventStore::new(temper_store_sim::SimEventStore::no_faults(27));
    let result = QueryPlaneStore::reconciled_equality_candidates(
        &store,
        &journal,
        "default",
        "Order",
        &[("Notes".into(), "needle".into())],
        Some(100),
    )
    .await
    .unwrap();
    assert!(
        result.is_none(),
        "a plane's empty delta says nothing about a different journal"
    );
}

#[tokio::test]
async fn equality_delta_keeps_native_paging_for_broad_caught_up_filters() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("broad.db").display()),
        None,
    )
    .await
    .unwrap();
    let tenant = TenantId::default();
    for i in 0..12 {
        upsert_order_projection(
            &store,
            &tenant,
            &format!("ord-{i:02}"),
            serde_json::json!({"Notes": "noise"}),
            1,
        )
        .await;
    }
    let state = build_order_state_with_store("delta-broad", &store);
    let security = SecurityContext::system();
    let options = QueryOptions {
        count: None,
        top: Some(1),
        ..notes("noise")
    };
    let result = read_entity_set_page(QueryPlaneReadRequest {
        state: &state,
        tenant: &tenant,
        security_ctx: &security,
        entity_type: "Order",
        entity_set_name: "Orders",
        query_options: &options,
        budget: QueryPlaneReadBudget {
            default_page_size: 1,
            max_entities: 1,
        },
    })
    .await
    .unwrap_or_else(|_| panic!("a caught-up broad filter must keep native paging"));
    assert_eq!(result.entities.len(), 1);
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::NativePagePushdown
    );
}

#[tokio::test]
async fn equality_delta_keeps_unprojected_matches_on_continuation_pages() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("pages.db").display()),
        None,
    )
    .await
    .unwrap();
    let state = build_order_state_with_store("delta-pages", &store);
    let tenant = TenantId::default();
    for id in ["ord-00", "ord-01", "ord-02"] {
        let result = state
            .dispatch_tenant_action(
                &tenant,
                "Order",
                id,
                "AddItem",
                serde_json::json!({"ProductId": "p", "Quantity": 1, "Notes": "needle"}),
                &AgentContext::for_service("delta-pages"),
            )
            .await
            .unwrap();
        assert!(result.success);
    }
    store
        .remove_query_projection(tenant.as_str(), "Order", "ord-01")
        .await
        .unwrap();
    let security = SecurityContext::system();
    let mut options = notes("needle");
    let mut ids = Vec::new();
    loop {
        let result = read_entity_set_from_query_plane(QueryPlaneReadRequest {
            state: &state,
            tenant: &tenant,
            security_ctx: &security,
            entity_type: "Order",
            entity_set_name: "Orders",
            query_options: &options,
            budget: QueryPlaneReadBudget {
                default_page_size: 1,
                max_entities: 10,
            },
        })
        .await
        .unwrap_or_else(|_| panic!("lagging continuation must answer"));
        assert_eq!(result.count, Some(3));
        ids.extend(
            result
                .entities
                .iter()
                .map(|entity| entity["entity_id"].as_str().unwrap().to_string()),
        );
        if result.next_skiptoken.is_none() {
            break;
        }
        options.skiptoken = result.next_skiptoken;
        assert!(ids.len() <= 3, "continuation must make progress");
    }
    assert_eq!(ids, vec!["ord-00", "ord-01", "ord-02"]);
}
