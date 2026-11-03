//! Equality conjunctions used to reconcile indexed reads and resolve keys.

use temper_odata::query::types::{BinaryOperator, FilterExpr, ODataValue};

use super::property_literal_pair;

/// Detect whether `filter` is a pure conjunction (`AND`-tree) of
/// `Property eq <scalar literal>` leaves — the shape of resolution lookups such
/// as `Path eq '..' and WorkspaceId eq '..'` and the directory variant
/// `Name eq '..' and WorkspaceId eq '..' and ParentId eq null`.
///
/// Returns the matched `(field, rendered_value)` pairs when the WHOLE
/// expression has that shape, or `None` for anything else (`Or`, `Ne`, ranges,
/// functions, unary, bare property/literal, or a non-scalar literal).
///
/// Used by the query plane to decide when an EMPTY native projection page for
/// an exact-match resolution is untrustworthy and must be reconciled against
/// authoritative state — a just-committed entity may not yet have its
/// asynchronously-projected row (ARN-89). `null`-equality leaves count as
/// equality here; the authoritative read re-evaluates the original filter, so
/// null semantics stay correct.
pub(in crate::odata) fn equality_field_predicates(
    filter: &FilterExpr,
) -> Option<Vec<(String, serde_json::Value)>> {
    let mut pairs = Vec::new();
    collect_equality_field_predicates(filter, &mut pairs).then_some(pairs)
}

/// Convert an OData literal to the JSON value the entity stores it as, so the
/// read-side key hash matches the write-side hash component-for-component.
/// `null` maps to `Value::Null` (a distinct, indexable key value).
fn odata_value_to_json(value: &ODataValue) -> Option<serde_json::Value> {
    use serde_json::Value;
    Some(match value {
        ODataValue::Null => Value::Null,
        ODataValue::Boolean(b) => Value::Bool(*b),
        ODataValue::Int(i) => Value::Number((*i).into()),
        ODataValue::Float(f) => Value::Number(serde_json::Number::from_f64(*f)?),
        ODataValue::String(s) => Value::String(s.clone()),
        ODataValue::Guid(g) => Value::String(g.to_string()),
        ODataValue::DateTimeOffset(dt) => Value::String(dt.to_rfc3339()),
    })
}

fn collect_equality_field_predicates(
    expr: &FilterExpr,
    out: &mut Vec<(String, serde_json::Value)>,
) -> bool {
    match expr {
        FilterExpr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            collect_equality_field_predicates(left, out)
                && collect_equality_field_predicates(right, out)
        }
        FilterExpr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } => {
            let Some((field_name, value)) = property_literal_pair(left, right) else {
                return false;
            };
            let Some(json) = odata_value_to_json(value) else {
                return false;
            };
            out.push((field_name.to_string(), json));
            true
        }
        _ => false,
    }
}
