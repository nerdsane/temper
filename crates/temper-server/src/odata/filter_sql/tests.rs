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
