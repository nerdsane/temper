use super::*;
use temper_odata::query::types::{FilterExpr, ODataValue};

#[test]
fn simple_eq_filter() {
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Status".into())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String("Active".into()))),
    };
    let result = try_translate_filter(&filter).unwrap();
    assert_eq!(result.where_clause, "status = ?3");
    assert_eq!(result.params, vec!["Active"]);
}

#[test]
fn lowercase_status_uses_catalog_status_column() {
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("status".into())),
        op: BinaryOperator::Ne,
        right: Box::new(FilterExpr::Literal(ODataValue::String("Archived".into()))),
    };
    let result = try_translate_filter(&filter).unwrap();
    assert_eq!(result.where_clause, "status != ?3");
    assert_eq!(result.params, vec!["Archived"]);
}

#[test]
fn ordinary_field_eq_filter_uses_field_index() {
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Priority".into())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::String("High".into()))),
    };
    let result = try_translate_filter(&filter).unwrap();
    assert!(result.where_clause.contains("field_name = ?3"));
    assert!(result.where_clause.contains("field_value = ?4"));
    assert_eq!(result.params, vec!["Priority", "High"]);
}

#[test]
fn and_combinator() {
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Status".into())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String("Active".into()))),
        }),
        op: BinaryOperator::And,
        right: Box::new(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Priority".into())),
            op: BinaryOperator::Gt,
            right: Box::new(FilterExpr::Literal(ODataValue::Int(5))),
        }),
    };
    let result = try_translate_filter(&filter).unwrap();
    assert!(result.where_clause.contains("AND"));
    assert_eq!(result.params, vec!["Active", "Priority", "5"]);
}

#[test]
fn candidate_and_equality_filter_is_lossless_for_string_fields() {
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("SessionId".into())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(
                "session-hot".into(),
            ))),
        }),
        op: BinaryOperator::And,
        right: Box::new(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("EntryId".into())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String("entry-1199".into()))),
        }),
    };

    let result = try_translate_candidate_filter(&filter).unwrap();
    assert!(result.where_clause.contains("AND"));
    assert!(result.where_clause.contains("field_name = ?3"));
    assert!(result.where_clause.contains("field_value = ?4"));
    assert!(result.where_clause.contains("field_name = ?5"));
    assert!(result.where_clause.contains("field_value = ?6"));
    assert_eq!(
        result.params,
        vec!["SessionId", "session-hot", "EntryId", "entry-1199"]
    );
}

#[test]
fn candidate_and_filter_keeps_lossless_equalities_when_status_ne_is_present() {
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::Property("Path".into())),
                op: BinaryOperator::Eq,
                right: Box::new(FilterExpr::Literal(ODataValue::String(
                    "/proofs/live.txt".into(),
                ))),
            }),
            op: BinaryOperator::And,
            right: Box::new(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::Property("WorkspaceId".into())),
                op: BinaryOperator::Eq,
                right: Box::new(FilterExpr::Literal(ODataValue::String("ws-1".into()))),
            }),
        }),
        op: BinaryOperator::And,
        right: Box::new(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Status".into())),
            op: BinaryOperator::Ne,
            right: Box::new(FilterExpr::Literal(ODataValue::String("Archived".into()))),
        }),
    };

    let result = try_translate_candidate_filter(&filter).unwrap();
    assert!(result.where_clause.contains("AND"));
    assert!(result.where_clause.contains("field_name = ?3"));
    assert!(result.where_clause.contains("field_value = ?4"));
    assert!(result.where_clause.contains("field_name = ?5"));
    assert!(result.where_clause.contains("field_value = ?6"));
    assert!(!result.where_clause.contains("status !="));
    assert_eq!(
        result.params,
        vec!["Path", "/proofs/live.txt", "WorkspaceId", "ws-1"]
    );
}

#[test]
fn candidate_or_filter_does_not_drop_non_lossless_branch() {
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Path".into())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String(
                "/proofs/live.txt".into(),
            ))),
        }),
        op: BinaryOperator::Or,
        right: Box::new(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Status".into())),
            op: BinaryOperator::Ne,
            right: Box::new(FilterExpr::Literal(ODataValue::String("Archived".into()))),
        }),
    };

    assert!(try_translate_candidate_filter(&filter).is_none());
}

#[test]
fn startswith_function() {
    let filter = FilterExpr::FunctionCall {
        name: "startswith".into(),
        args: vec![
            FilterExpr::Property("path".into()),
            FilterExpr::Literal(ODataValue::String("/system/skills/".into())),
        ],
    };
    let result = try_translate_filter(&filter).unwrap();
    assert!(result.where_clause.contains("LIKE"));
    assert_eq!(result.params[0], "path");
    assert_eq!(result.params[1], "/system/skills/%");
}

#[test]
fn contains_function() {
    let filter = FilterExpr::FunctionCall {
        name: "contains".into(),
        args: vec![
            FilterExpr::Property("Name".into()),
            FilterExpr::Literal(ODataValue::String("test".into())),
        ],
    };
    let result = try_translate_filter(&filter).unwrap();
    assert!(result.where_clause.contains("LIKE"));
    assert_eq!(result.params[1], "%test%");
}

#[test]
fn endswith_function() {
    let filter = FilterExpr::FunctionCall {
        name: "endswith".into(),
        args: vec![
            FilterExpr::Property("Name".into()),
            FilterExpr::Literal(ODataValue::String(".md".into())),
        ],
    };
    let result = try_translate_filter(&filter).unwrap();
    assert_eq!(result.params[1], "%.md");
}

#[test]
fn not_combinator() {
    let filter = FilterExpr::UnaryOp {
        op: UnaryOperator::Not,
        operand: Box::new(FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Status".into())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String("Deleted".into()))),
        }),
    };
    let result = try_translate_filter(&filter).unwrap();
    assert!(result.where_clause.starts_with("(NOT "));
}

#[test]
fn null_eq_filter() {
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Description".into())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::Null)),
    };
    let result = try_translate_filter(&filter).unwrap();
    assert!(result.where_clause.contains("IS NULL"));
    assert!(try_translate_candidate_filter(&filter).is_none());
}

#[test]
fn status_null_filter_uses_catalog_status_column() {
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Status".into())),
        op: BinaryOperator::Ne,
        right: Box::new(FilterExpr::Literal(ODataValue::Null)),
    };
    let result = try_translate_filter(&filter).unwrap();
    assert_eq!(result.where_clause, "status IS NOT NULL");
    assert!(result.params.is_empty());
    assert!(try_translate_candidate_filter(&filter).is_none());
}

#[test]
fn status_null_equality_is_lossless_candidate_filter() {
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Status".into())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(ODataValue::Null)),
    };
    let result = try_translate_candidate_filter(&filter).unwrap();
    assert_eq!(result.where_clause, "status IS NULL");
    assert!(result.params.is_empty());
}

#[test]
fn unsupported_has_returns_none() {
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Flags".into())),
        op: BinaryOperator::Has,
        right: Box::new(FilterExpr::Literal(ODataValue::String("Admin".into()))),
    };
    assert!(try_translate_filter(&filter).is_none());
}

#[test]
fn cross_field_comparison_returns_none() {
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("A".into())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Property("B".into())),
    };
    assert!(try_translate_filter(&filter).is_none());
}

#[test]
fn like_wildcards_escaped() {
    let filter = FilterExpr::FunctionCall {
        name: "contains".into(),
        args: vec![
            FilterExpr::Property("Name".into()),
            FilterExpr::Literal(ODataValue::String("100%".into())),
        ],
    };
    let result = try_translate_filter(&filter).unwrap();
    assert_eq!(result.params[1], "%100\\%%");
}

// --- equality_field_predicates (ARN-89 reconcile detection) ---
//
// `equality_field_predicates` decides when an EMPTY native projection page is
// untrustworthy under projection lag. It returns `Some(pairs)` ONLY when the
// WHOLE filter is an AND-tree of `Property eq <scalar literal | null>` leaves —
// the shape of a TemperFS resolution lookup. Anything else (ranges, `ne`, `or`,
// functions, bare props) must return `None` so the hot path is never penalized.

fn eq(field: &str, value: ODataValue) -> FilterExpr {
    FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property(field.into())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Literal(value)),
    }
}

fn and(left: FilterExpr, right: FilterExpr) -> FilterExpr {
    FilterExpr::BinaryOp {
        left: Box::new(left),
        op: BinaryOperator::And,
        right: Box::new(right),
    }
}

#[test]
fn equality_predicates_accepts_two_scalar_conjuncts() {
    // `Path eq '/proofs/live.txt' and WorkspaceId eq 'ws-1'`
    let filter = and(
        eq("Path", ODataValue::String("/proofs/live.txt".into())),
        eq("WorkspaceId", ODataValue::String("ws-1".into())),
    );
    let pairs = equality_field_predicates(&filter).expect("pure equality conjunction");
    assert_eq!(
        pairs,
        vec![
            ("Path".to_string(), "/proofs/live.txt".to_string()),
            ("WorkspaceId".to_string(), "ws-1".to_string()),
        ]
    );
}

#[test]
fn equality_predicates_accepts_null_leaf_in_directory_conjunction() {
    // `Name eq 'inbox' and WorkspaceId eq 'ws-1' and ParentId eq null` — the
    // directory-resolution variant. A null leaf still counts as equality; the
    // authoritative read re-evaluates the original filter so null semantics
    // stay correct.
    let filter = and(
        and(
            eq("Name", ODataValue::String("inbox".into())),
            eq("WorkspaceId", ODataValue::String("ws-1".into())),
        ),
        eq("ParentId", ODataValue::Null),
    );
    let pairs = equality_field_predicates(&filter).expect("null leaf is accepted");
    assert_eq!(
        pairs,
        vec![
            ("Name".to_string(), "inbox".to_string()),
            ("WorkspaceId".to_string(), "ws-1".to_string()),
            // null renders as the empty string sentinel.
            ("ParentId".to_string(), String::new()),
        ]
    );
}

#[test]
fn equality_predicates_rejects_range_predicate() {
    // `Total gt 5` is not equality.
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Total".into())),
        op: BinaryOperator::Gt,
        right: Box::new(FilterExpr::Literal(ODataValue::Int(5))),
    };
    assert!(equality_field_predicates(&filter).is_none());
}

#[test]
fn equality_predicates_rejects_ne_predicate() {
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("Status".into())),
        op: BinaryOperator::Ne,
        right: Box::new(FilterExpr::Literal(ODataValue::String("Archived".into()))),
    };
    assert!(equality_field_predicates(&filter).is_none());
}

#[test]
fn equality_predicates_rejects_or_conjunction() {
    // One `Or` anywhere in the tree poisons the whole expression.
    let filter = FilterExpr::BinaryOp {
        left: Box::new(eq("Path", ODataValue::String("/a".into()))),
        op: BinaryOperator::Or,
        right: Box::new(eq("Path", ODataValue::String("/b".into()))),
    };
    assert!(equality_field_predicates(&filter).is_none());
}

#[test]
fn equality_predicates_rejects_function_call() {
    let filter = FilterExpr::FunctionCall {
        name: "startswith".into(),
        args: vec![
            FilterExpr::Property("Path".into()),
            FilterExpr::Literal(ODataValue::String("/proofs/".into())),
        ],
    };
    assert!(equality_field_predicates(&filter).is_none());
}

#[test]
fn equality_predicates_rejects_function_call_inside_conjunction() {
    // A single non-equality conjunct disqualifies the whole AND-tree.
    let filter = and(
        eq("WorkspaceId", ODataValue::String("ws-1".into())),
        FilterExpr::FunctionCall {
            name: "startswith".into(),
            args: vec![
                FilterExpr::Property("Path".into()),
                FilterExpr::Literal(ODataValue::String("/proofs/".into())),
            ],
        },
    );
    assert!(equality_field_predicates(&filter).is_none());
}

#[test]
fn equality_predicates_rejects_bare_property() {
    assert!(equality_field_predicates(&FilterExpr::Property("Path".into())).is_none());
}

#[test]
fn equality_predicates_rejects_bare_literal() {
    assert!(equality_field_predicates(&FilterExpr::Literal(ODataValue::Boolean(true))).is_none());
}

#[test]
fn equality_predicates_rejects_cross_field_equality() {
    // `A eq B` (property == property) is not a scalar-literal leaf.
    let filter = FilterExpr::BinaryOp {
        left: Box::new(FilterExpr::Property("A".into())),
        op: BinaryOperator::Eq,
        right: Box::new(FilterExpr::Property("B".into())),
    };
    assert!(equality_field_predicates(&filter).is_none());
}
