use super::*;

#[test]
fn decimals_use_numeric_not_lexical_order() {
    assert_eq!(compare_decimal("10", "2"), Some(Ordering::Greater));
    assert_eq!(compare_decimal("-10", "-2"), Some(Ordering::Less));
    assert_eq!(compare_decimal("2.0", "2.00"), Some(Ordering::Equal));
}

#[test]
fn date_times_compare_as_instants() {
    let actual = serde_json::json!("2026-01-01T01:00:00+01:00");
    let expected = ScalarV1::DateTimeOffset("2026-01-01T00:00:00Z".into());
    assert_eq!(
        compare_scalar(Some(&actual), &expected),
        Some(Ordering::Equal)
    );
}

#[test]
fn fallback_order_matches_declared_direction_and_null_rules() {
    let response = |id: &str, estimate: Value| crate::entity_actor::EntityResponse {
        success: true,
        state: crate::entity_actor::EntityState {
            entity_type: "Task".into(),
            entity_id: id.into(),
            status: "Open".into(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields: serde_json::json!({"Estimate": estimate}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr: 0,
            processed_idempotency_keys: BTreeMap::new(),
        },
        error: None,
        custom_effects: Vec::new(),
        scheduled_actions: Vec::new(),
        spawn_requests: Vec::new(),
        spec_governed: true,
    };
    let schema = ManifestEntityV1 {
        entity_type: "Temper.Task".into(),
        entity_set: "Tasks".into(),
        generated_name: "Task".into(),
        properties: vec![temper_wasm_sdk::data::ManifestPropertyV1 {
            canonical_name: "Estimate".into(),
            generated_name: "estimate".into(),
            type_name: "Edm.Decimal".into(),
            nullable: true,
            enum_members: Vec::new(),
        }],
        actions: Vec::new(),
    };
    let low = response("a", serde_json::json!("2"));
    let high = response("b", serde_json::json!("10"));
    assert_eq!(
        compare_fallback_entities(
            "a",
            Some(&low),
            "b",
            Some(&high),
            &[OrderV1::property("Estimate", OrderDirectionV1::Asc)],
            &schema
        ),
        Ordering::Less
    );
}

#[test]
fn fallback_orders_by_host_owned_commit_sequence() {
    let response = |id: &str, sequence_nr| crate::entity_actor::EntityResponse {
        success: true,
        state: crate::entity_actor::EntityState {
            entity_type: "Task".into(),
            entity_id: id.into(),
            status: "Open".into(),
            item_count: 0,
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
            fields: serde_json::json!({"sequence_nr": 999}),
            events: std::collections::VecDeque::new(),
            total_event_count: 0,
            events_since_snapshot: 0,
            last_snapshot_sequence_nr: 0,
            sequence_nr,
            processed_idempotency_keys: BTreeMap::new(),
        },
        error: None,
        custom_effects: Vec::new(),
        scheduled_actions: Vec::new(),
        spawn_requests: Vec::new(),
        spec_governed: true,
    };
    let schema = ManifestEntityV1 {
        entity_type: "Temper.Task".into(),
        entity_set: "Tasks".into(),
        generated_name: "Task".into(),
        properties: Vec::new(),
        actions: Vec::new(),
    };
    let older = response("a", 3);
    let newer = response("b", 7);
    assert_eq!(
        compare_fallback_entities(
            "a",
            Some(&older),
            "b",
            Some(&newer),
            &[OrderV1::EntityCommitSequence {
                direction: OrderDirectionV1::Desc,
            }],
            &schema,
        ),
        Ordering::Greater
    );
}

#[test]
fn closed_filter_evaluates_every_scalar_operator_and_null() {
    let value = serde_json::json!({
        "Boolean": true, "Int": 7, "Double": 2.5, "String": "bravo",
        "Guid": "018f1f80-7b2d-7000-8000-000000000001",
        "When": "2026-01-01T00:00:00Z", "Decimal": "10.25", "Enum": "Open", "Nullable": null
    });
    let object = value.as_object().unwrap();
    let comparisons = [
        ("Boolean", CompareOperatorV1::Eq, ScalarV1::Boolean(true)),
        ("Int", CompareOperatorV1::Gt, ScalarV1::Int64(6)),
        ("Int", CompareOperatorV1::Ge, ScalarV1::Int64(7)),
        ("Double", CompareOperatorV1::Lt, ScalarV1::Double(3.0)),
        (
            "String",
            CompareOperatorV1::Le,
            ScalarV1::String("bravo".into()),
        ),
        (
            "Guid",
            CompareOperatorV1::Ne,
            ScalarV1::Guid("018f1f80-7b2d-7000-8000-000000000002".into()),
        ),
        (
            "When",
            CompareOperatorV1::Eq,
            ScalarV1::DateTimeOffset("2025-12-31T19:00:00-05:00".into()),
        ),
        (
            "Decimal",
            CompareOperatorV1::Gt,
            ScalarV1::Decimal("2".into()),
        ),
        (
            "Enum",
            CompareOperatorV1::Eq,
            ScalarV1::Enum(temper_wasm_sdk::data::EnumValueV1 {
                type_name: "Temper.Status".into(),
                member: "Open".into(),
            }),
        ),
    ];
    for (field, operator, expected) in comparisons {
        assert!(matches_filter(
            &FilterV1::Compare {
                field: field.into(),
                operator,
                value: expected
            },
            object
        ));
    }
    assert!(matches_filter(
        &FilterV1::IsNull {
            field: "Nullable".into(),
            is_null: true
        },
        object
    ));
    assert!(matches_filter(
        &FilterV1::Not {
            operand: Box::new(FilterV1::IsNull {
                field: "String".into(),
                is_null: true,
            })
        },
        object
    ));
}
