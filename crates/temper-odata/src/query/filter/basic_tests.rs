use super::*;

#[test]
fn filter_simple_eq_string() {
    let expr = parse_filter("Name eq 'foo'").unwrap();
    assert_eq!(
        expr,
        FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Name".into())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::String("foo".into()))),
        }
    );
}

#[test]
fn filter_simple_gt_float() {
    let expr = parse_filter("Price gt 5.0").unwrap();
    assert_eq!(
        expr,
        FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Price".into())),
            op: BinaryOperator::Gt,
            right: Box::new(FilterExpr::Literal(ODataValue::Float(5.0))),
        }
    );
}

#[test]
fn filter_and_or_precedence() {
    // `A eq 1 and B eq 2 or C eq 3` should parse as `(A eq 1 and B eq 2) or (C eq 3)`
    let expr = parse_filter("A eq 1 and B eq 2 or C eq 3").unwrap();
    match &expr {
        FilterExpr::BinaryOp {
            op: BinaryOperator::Or,
            left,
            right,
        } => {
            // Left should be the 'and' node
            match left.as_ref() {
                FilterExpr::BinaryOp {
                    op: BinaryOperator::And,
                    ..
                } => {}
                other => panic!("expected And on left, got {other:?}"),
            }
            // Right should be a comparison
            match right.as_ref() {
                FilterExpr::BinaryOp {
                    op: BinaryOperator::Eq,
                    ..
                } => {}
                other => panic!("expected Eq on right, got {other:?}"),
            }
        }
        other => panic!("expected Or at top, got {other:?}"),
    }
}

#[test]
fn filter_compound_and() {
    let expr = parse_filter("Name eq 'foo' and Price gt 5.0").unwrap();
    assert_eq!(
        expr,
        FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::Property("Name".into())),
                op: BinaryOperator::Eq,
                right: Box::new(FilterExpr::Literal(ODataValue::String("foo".into()))),
            }),
            op: BinaryOperator::And,
            right: Box::new(FilterExpr::BinaryOp {
                left: Box::new(FilterExpr::Property("Price".into())),
                op: BinaryOperator::Gt,
                right: Box::new(FilterExpr::Literal(ODataValue::Float(5.0))),
            }),
        }
    );
}

#[test]
fn filter_not_operator() {
    let expr = parse_filter("not Active eq true").unwrap();
    match &expr {
        FilterExpr::UnaryOp {
            op: UnaryOperator::Not,
            operand,
        } => match operand.as_ref() {
            FilterExpr::BinaryOp {
                op: BinaryOperator::Eq,
                ..
            } => {}
            other => panic!("expected Eq inside not, got {other:?}"),
        },
        other => panic!("expected Not at top, got {other:?}"),
    }
}

#[test]
fn filter_parenthesized_expression() {
    let expr = parse_filter("(A eq 1 or B eq 2) and C eq 3").unwrap();
    match &expr {
        FilterExpr::BinaryOp {
            op: BinaryOperator::And,
            left,
            ..
        } => match left.as_ref() {
            FilterExpr::BinaryOp {
                op: BinaryOperator::Or,
                ..
            } => {}
            other => panic!("expected Or in parens, got {other:?}"),
        },
        other => panic!("expected And at top, got {other:?}"),
    }
}

#[test]
fn filter_function_call() {
    let expr = parse_filter("contains(Name, 'foo')").unwrap();
    assert_eq!(
        expr,
        FilterExpr::FunctionCall {
            name: "contains".into(),
            args: vec![
                FilterExpr::Property("Name".into()),
                FilterExpr::Literal(ODataValue::String("foo".into())),
            ],
        }
    );
}

#[test]
fn filter_null_literal() {
    let expr = parse_filter("Name eq null").unwrap();
    assert_eq!(
        expr,
        FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Name".into())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::Null)),
        }
    );
}

#[test]
fn filter_boolean_literal() {
    let expr = parse_filter("Active eq true").unwrap();
    assert_eq!(
        expr,
        FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Active".into())),
            op: BinaryOperator::Eq,
            right: Box::new(FilterExpr::Literal(ODataValue::Boolean(true))),
        }
    );
}

#[test]
fn filter_negative_number() {
    let expr = parse_filter("Amount gt -10").unwrap();
    assert_eq!(
        expr,
        FilterExpr::BinaryOp {
            left: Box::new(FilterExpr::Property("Amount".into())),
            op: BinaryOperator::Gt,
            right: Box::new(FilterExpr::Literal(ODataValue::Int(-10))),
        }
    );
}
