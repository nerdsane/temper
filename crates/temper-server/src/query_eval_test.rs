use super::*;

fn sample_entities() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({"Id": "1", "Name": "Alice", "Age": 30, "Status": "Active"}),
        serde_json::json!({"Id": "2", "Name": "Bob", "Age": 25, "Status": "Draft"}),
        serde_json::json!({"Id": "3", "Name": "Charlie", "Age": 35, "Status": "Active"}),
    ]
}

#[test]
fn test_filter_eq() {
    let entities = sample_entities();
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Status".into())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String("Active".into()))),
    };
    let filtered = filter_entities(entities, &filter);
    assert_eq!(filtered.len(), 2);
    assert_eq!(filtered[0]["Name"], "Alice");
    assert_eq!(filtered[1]["Name"], "Charlie");
}

/// ARN-68 regression: `prop eq null` must match a row that OMITS the property —
/// a Directory root has no `ParentId` field. Previously the absent operand made
/// `evaluate_value` return `None` and the `?` collapsed the WHOLE compound filter
/// to false, dropping every root and causing `ensure_dirs` to recreate roots
/// (the duplicate-root bug). This mirrors the native `WHERE ParentId IS NULL`.
#[test]
fn root_lookup_eq_null_matches_absent_property_in_compound_filter() {
    // The real `ensure_dirs` root lookup shape, against a root with no `ParentId`.
    let root = serde_json::json!({
        "Id": "r1", "fields": {"Name": "/", "WorkspaceId": "wsA", "Status": "Active"}
    });
    let subdir = serde_json::json!({
        "Id": "d1", "fields": {"Name": "/", "WorkspaceId": "wsA", "ParentId": "r1"}
    });
    let eq = |p: &str, v: &str| FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property(p.into())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String(v.into()))),
    };
    let parentid_eq_null = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("ParentId".into())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::Null)),
    };
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::BinaryOp {
            left: Box::new(eq("Name", "/")),
            op: BinaryOperator::And,
            right: Box::new(eq("WorkspaceId", "wsA")),
        }),
        op: BinaryOperator::And,
        right: Box::new(parentid_eq_null),
    };
    let matched = filter_entities(vec![root, subdir], &filter);
    assert_eq!(matched.len(), 1, "only the root (absent ParentId) matches");
    assert_eq!(matched[0]["Id"], "r1");
}

/// `eq`/`ne` null semantics match the native SQL pushdown (IS NULL / IS NOT NULL).
#[test]
fn null_comparison_semantics_match_sql() {
    use serde_json::Value;
    let s = |x: &str| Value::String(x.into());
    // eq: NULL eq NULL → true (IS NULL); present eq null → false; value equality.
    assert!(compare_nullable(
        None,
        Some(&Value::Null),
        &BinaryOperator::Eq
    ));
    assert!(!compare_nullable(
        Some(&s("x")),
        Some(&Value::Null),
        &BinaryOperator::Eq
    ));
    assert!(compare_nullable(
        Some(&s("x")),
        Some(&s("x")),
        &BinaryOperator::Eq
    ));
    // ne: present ne null → true (IS NOT NULL); absent ne null → false; value ne.
    assert!(compare_nullable(
        Some(&s("x")),
        Some(&Value::Null),
        &BinaryOperator::Ne
    ));
    assert!(!compare_nullable(
        None,
        Some(&Value::Null),
        &BinaryOperator::Ne
    ));
    assert!(compare_nullable(
        Some(&s("x")),
        Some(&s("y")),
        &BinaryOperator::Ne
    ));
    // ordering with a null operand → excluded (UNKNOWN).
    assert!(!compare_nullable(
        None,
        Some(&Value::Null),
        &BinaryOperator::Gt
    ));
}

#[test]
fn status_filter_matches_catalog_status_aliases() {
    let entities = vec![
        serde_json::json!({"Id": "1", "status": "Created", "fields": {"Name": "Ready"}}),
        serde_json::json!({"Id": "2", "status": "Archived", "fields": {"Name": "Old"}}),
        serde_json::json!({"Id": "3", "fields": {"status": "Created", "Name": "Nested"}}),
    ];
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Status".into())),
        op: BinaryOperator::Ne,
        right: Box::new(FilterExpr::Literal(ODataValue::String("Archived".into()))),
    };
    let filtered = filter_entities(entities, &filter);

    assert_eq!(filtered.len(), 2);
    assert_eq!(filtered[0]["Id"], "1");
    assert_eq!(filtered[1]["Id"], "3");
}

#[test]
fn test_filter_gt() {
    let entities = sample_entities();
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Age".into())),
        op: BinaryOperator::Gt,
        right: Box::new(FilterExpr::Literal(ODataValue::Int(28))),
    };
    let filtered = filter_entities(entities, &filter);
    assert_eq!(filtered.len(), 2);
}

#[test]
fn test_orderby_asc() {
    let mut entities = sample_entities();
    let orderby = vec![OrderByClause {
        property: "Age".into(),
        direction: OrderDirection::Asc,
    }];
    sort_entities(&mut entities, &orderby);
    assert_eq!(entities[0]["Name"], "Bob");
    assert_eq!(entities[1]["Name"], "Alice");
    assert_eq!(entities[2]["Name"], "Charlie");
}

#[test]
fn test_orderby_desc() {
    let mut entities = sample_entities();
    let orderby = vec![OrderByClause {
        property: "Name".into(),
        direction: OrderDirection::Desc,
    }];
    sort_entities(&mut entities, &orderby);
    assert_eq!(entities[0]["Name"], "Charlie");
    assert_eq!(entities[1]["Name"], "Bob");
    assert_eq!(entities[2]["Name"], "Alice");
}

#[test]
fn test_select_fields() {
    let entities = sample_entities();
    let selected = select_fields(entities, &["Id".into(), "Name".into()]);
    assert_eq!(selected[0].as_object().unwrap().len(), 2);
    assert!(selected[0].get("Id").is_some());
    assert!(selected[0].get("Name").is_some());
    assert!(selected[0].get("Age").is_none());
}

#[test]
fn test_apply_query_options_combined() {
    let entities = sample_entities();
    let options = QueryOptions {
        filter: Some(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Status".into())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String("Active".into()))),
        }),
        orderby: Some(vec![OrderByClause {
            property: "Name".into(),
            direction: OrderDirection::Asc,
        }]),
        top: Some(1),
        skip: None,
        select: Some(vec!["Id".into(), "Name".into()]),
        count: Some(true),
        expand: None,
        skiptoken: None,
    };

    let (result, count) = apply_query_options(entities, &options);
    assert_eq!(count, Some(2)); // 2 Active entities before pagination
    assert_eq!(result.len(), 1); // $top=1
    assert_eq!(result[0]["Name"], "Alice"); // First alphabetically among Active
}

#[test]
fn test_contains_function() {
    let entities = sample_entities();
    let filter = FilterExpr::FunctionCall {
        name: "contains".into(),
        args: vec![
            FilterExpr::Property("Name".into()),
            FilterExpr::Literal(ODataValue::String("li".into())),
        ],
    };
    let filtered = filter_entities(entities, &filter);
    assert_eq!(filtered.len(), 2); // Alice and Charlie
}

#[test]
fn test_find_fk_resolution_forward() {
    // Simulate Order→Customer: Order has outgoing edge with source_field=CustomerId
    let mut graph = crate::registry::RelationGraph::default();
    graph.outgoing.insert(
        "Order".to_string(),
        vec![crate::registry::RelationEdge {
            from_entity: "Order".to_string(),
            navigation_property: "Customer".to_string(),
            to_entity: "Customer".to_string(),
            source_field: "CustomerId".to_string(),
            target_field: "Id".to_string(),
            nullable: false,
            delete_policy: temper_spec::cross_invariant::DeletePolicy::Restrict,
        }],
    );

    // Non-collection nav → Forward resolution
    let result = find_fk_resolution(&graph, "Order", "Customer", "Customer", false);
    assert!(result.is_some());
    match result.unwrap() {
        FkResolution::Forward { source_field } => {
            assert_eq!(source_field, "CustomerId");
        }
        _ => panic!("Expected Forward resolution"),
    }
}

#[test]
fn test_find_fk_resolution_reverse() {
    // Simulate Customer→Orders: Order has outgoing edge back to Customer
    let mut graph = crate::registry::RelationGraph::default();
    graph.outgoing.insert(
        "Order".to_string(),
        vec![crate::registry::RelationEdge {
            from_entity: "Order".to_string(),
            navigation_property: "Customer".to_string(),
            to_entity: "Customer".to_string(),
            source_field: "CustomerId".to_string(),
            target_field: "Id".to_string(),
            nullable: false,
            delete_policy: temper_spec::cross_invariant::DeletePolicy::Restrict,
        }],
    );

    // Collection nav on Customer→Orders → Reverse resolution
    let result = find_fk_resolution(&graph, "Customer", "Order", "Orders", true);
    assert!(result.is_some());
    match result.unwrap() {
        FkResolution::Reverse { target_fk_field } => {
            assert_eq!(target_fk_field, "CustomerId");
        }
        _ => panic!("Expected Reverse resolution"),
    }
}

#[test]
fn test_find_fk_resolution_no_edge() {
    let graph = crate::registry::RelationGraph::default();
    // No edges → fallback
    let result = find_fk_resolution(&graph, "Foo", "Bar", "Bars", true);
    assert!(result.is_none());
}
