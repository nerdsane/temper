use super::*;

#[tokio::test]
async fn indexed_sort_with_declared_defaults_stays_bounded_when_values_are_present() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("indexed-default.db").display()),
        None,
    )
    .await
    .unwrap();
    let mut state = build_order_state("indexed-declared-default");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let spec = format!(
        "{ORDER_IOA}\n[[state]]\nname = \"Currency\"\ntype = \"string\"\ninitial = \"USD\"\n"
    );
    state.registry.write().unwrap().register_tenant(
        "default",
        parse_csdl(CSDL_XML).unwrap(),
        CSDL_XML.to_string(),
        &[("Order", &spec)],
    );
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let tenant = TenantId::default();
    // More records than the read-source scan budget below. Every sort value
    // is indexed, so declaring an initial value must not force a full scan.
    for index in 0..12 {
        upsert_order_projection(
            &store,
            &tenant,
            &format!("ord-{index:02}"),
            serde_json::json!({"Currency": if index == 0 { "EUR" } else { "USD" },
                "TotalAmount": if index == 1 { 1 } else { 0 }}),
            1,
        )
        .await;
    }
    // A missing value in another tenant must not disable this tenant's indexed count.
    upsert_order_projection(
        &store,
        &TenantId::from("other"),
        "ord-other",
        serde_json::json!({}),
        1,
    )
    .await;
    let security = SecurityContext::system();
    let options = QueryOptions {
        orderby: Some(vec![OrderByClause {
            property: "Currency".into(),
            direction: OrderDirection::Asc,
        }]),
        top: Some(1),
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Currency".into())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String("EUR".into()))),
        }),
        ..QueryOptions::default()
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
    .unwrap_or_else(|error| {
        panic!(
            "complete indexed sort failed: {}",
            error.into_response().status()
        )
    });
    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["entity_id"], "ord-00");
    assert_eq!(result.count, None);
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::NativePagePushdown
    );
    let mut options = options;
    options.count = Some(true);
    options.skip = Some(1);
    options.filter = Some(FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Currency".into())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String("USD".into()))),
    });
    let result = read_entity_set_page(QueryPlaneReadRequest {
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
    .unwrap_or_else(|error| {
        panic!(
            "default-matching candidate page failed: {}",
            error.into_response().status()
        )
    });
    assert_eq!(result.entities.len(), 1);
    assert_eq!(result.entities[0]["entity_id"], "ord-02");
    assert_eq!(result.count, Some(11));
    assert_eq!(result.telemetry.materialized_count, 1);
    // The native path can count beyond its materialization budget when every
    // candidate is indexed. The outer planner retains its separate proof budget.
    let native_request = QueryPlaneReadRequest {
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
    };
    let native_plan = native_candidate_page_plan(&native_request).unwrap();
    let native = try_native_page_read(
        &native_request,
        native_plan,
        QueryPlaneCoverageReport::default(),
    )
    .await
    .unwrap()
    .unwrap_or_else(|error| {
        panic!(
            "indexed native count failed: {}",
            error.into_response().status()
        )
    });
    assert_eq!(native.count, Some(11));
    assert_eq!(native.entities[0]["entity_id"], "ord-02");
    assert_eq!(native.telemetry.materialized_count, 1);
    // Skipping past all matches still needs a bounded indexed count, with
    // no documents materialized merely to distinguish an empty page.
    for include_count in [true, false] {
        let mut empty_options = options.clone();
        empty_options.skip = Some(11);
        empty_options.count = Some(include_count);
        let empty_request = QueryPlaneReadRequest {
            query_options: &empty_options,
            ..native_request
        };
        let empty = try_native_page_read(
            &empty_request,
            native_candidate_page_plan(&empty_request).unwrap(),
            QueryPlaneCoverageReport::default(),
        )
        .await
        .unwrap()
        .unwrap_or_else(|_| panic!("complete indexed count must stay bounded past the last match"));
        assert!(empty.entities.is_empty());
        assert_eq!(empty.count, include_count.then_some(11));
        assert_eq!(empty.telemetry.materialized_count, 0);
    }
    assert_eq!(
        result.telemetry.strategy,
        QueryPlaneReadStrategy::NativePagePushdown
    );
    // A complete index cannot make an untranslated conjunct exact.
    options.skip = None;
    options.filter =
        Some(temper_odata::query::parse_filter("Currency eq 'USD' and TotalAmount gt 0").unwrap());
    let result = read_entity_set_page(QueryPlaneReadRequest {
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
    .unwrap_or_else(|error| panic!("partial filter failed: {}", error.into_response().status()));
    assert_eq!(result.count, Some(1));
    assert_eq!(result.entities[0]["entity_id"], "ord-01");

    // Missing and oversized stored values both lack an index entry. Only
    // the genuinely missing value matches the declared default.
    for (id, fields) in [
        ("ord-missing", serde_json::json!({})),
        (
            "ord-large",
            serde_json::json!({"Currency": "x".repeat(2101)}),
        ),
    ] {
        upsert_order_projection(&store, &tenant, id, fields, 1).await;
    }
    options.orderby = None;
    options.filter = Some(temper_odata::query::parse_filter("Currency eq 'USD'").unwrap());
    let result = read_entity_set_page(QueryPlaneReadRequest {
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
    .unwrap_or_else(|error| {
        panic!(
            "missing filter values failed: {}",
            error.into_response().status()
        )
    });
    assert_eq!(result.count, Some(12));
    assert_eq!(result.entities[0]["entity_id"], "ord-01");
}

#[tokio::test]
async fn declared_default_equality_keeps_other_indexed_conjuncts() {
    let dir = tempfile::tempdir().unwrap();
    let store = TursoEventStore::new(
        &format!("file:{}", dir.path().join("default-conjunct.db").display()),
        None,
    )
    .await
    .unwrap();
    let mut state = build_order_state("default-conjunct");
    state.set_storage_stack(StorageStack::from_turso(store.clone()));
    let spec = format!(
        "{ORDER_IOA}\n[[state]]\nname = \"Currency\"\ntype = \"string\"\ninitial = \"USD\"\n"
    );
    state.registry.write().unwrap().register_tenant(
        "default",
        parse_csdl(CSDL_XML).unwrap(),
        CSDL_XML.to_string(),
        &[("Order", &spec)],
    );
    state
        .authz
        .reload_tenant_policies("default", "permit(principal, action, resource);")
        .unwrap();
    let tenant = TenantId::default();
    for (id, fields) in [
        ("ord-00-old", serde_json::json!({"Notes": "keep"})),
        (
            "ord-01-stored",
            serde_json::json!({"Notes": "keep", "Currency": "USD"}),
        ),
        (
            "ord-02-other-value",
            serde_json::json!({"Notes": "keep", "Currency": "EUR"}),
        ),
        ("ord-03-other-note", serde_json::json!({"Notes": "drop"})),
    ] {
        upsert_order_projection(&store, &tenant, id, fields, 1).await;
    }
    let security = SecurityContext::system();
    for (currency, count, first) in [("USD", 2, "ord-00-old"), ("EUR", 1, "ord-02-other-value")] {
        let options = QueryOptions {
            filter: Some(
                temper_odata::query::parse_filter(&format!(
                    "Currency eq '{currency}' and Notes eq 'keep'"
                ))
                .unwrap(),
            ),
            count: Some(true),
            top: Some(1),
            ..QueryOptions::default()
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
                max_entities: 10,
            },
        })
        .await
        .unwrap_or_else(|_| panic!("declared-default conjunction must be readable"));
        assert_eq!(result.count, Some(count));
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0]["entity_id"], first);
        assert_eq!(result.entities[0]["fields"]["Currency"], currency);
        assert!(result.telemetry.filter_pushdown);
    }
}
