//! Server-driven paging: keyset `$skiptoken` continuation for OData list reads.
//!
//! A list read truncated by the page size (default or `$top`-capped) must
//! advertise a continuation (`@odata.nextLink`) per OData v4, or a caller reads
//! the first page and concludes that is the whole set (ARN-160). We express the
//! continuation as a keyset cursor over the read's canonical ordering so that
//! following the link repeatedly enumerates the full result set with no
//! duplicates or gaps — on every backend (Postgres, Turso) and the in-memory
//! source-cursor path (the sim store).
//!
//! Canonical ordering = the request's `$orderby` clauses followed by `entity_id`
//! ascending as a total-order tiebreaker (the id both the native SQL page and the
//! in-memory sort already order by). The cursor records the ordering values of the
//! last returned row; the next page keeps only rows that sort strictly after it,
//! expressed as an ordinary `$filter` predicate so the existing read plan (native
//! pushdown narrowing + in-memory re-check) honors it unchanged.
//!
//! Null ordering matches the backends' `NULLS LAST` for ascending / `NULLS FIRST`
//! for descending, which is also what the in-memory sort produces (present values
//! sort before absent for ascending). The final `entity_id` key is never null, so
//! the tuple comparison always resolves to a strict order between distinct rows.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use temper_odata::query::types::{
    BinaryOperator, FilterExpr, ODataValue, OrderByClause, OrderDirection,
};

use crate::query_eval::resolve_property;

/// The always-present final tiebreaker property. Resolves to the entity's
/// top-level `entity_id`, which every materialized body carries.
pub(super) const ID_PROPERTY: &str = "entity_id";

/// One key of a decoded cursor: the boundary row's value for a pagination
/// clause. `present == false` means the row had no value there (SQL NULL).
#[derive(Clone, Debug, PartialEq)]
struct CursorKey {
    present: bool,
    value: serde_json::Value,
}

/// A decoded `$skiptoken`: the boundary row's value for each canonical
/// pagination clause, in clause order (orderby clauses, then `entity_id`).
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Cursor {
    keys: Vec<CursorKey>,
}

/// The canonical pagination ordering for a request: the caller's `$orderby`
/// clauses with `entity_id` ascending appended as a total-order tiebreaker.
///
/// The tiebreaker is appended unless the caller's last clause is already
/// `entity_id`, so the ordering is always a strict total order over distinct
/// entities (entity ids are unique).
pub(super) fn canonical_pagination_order(orderby: Option<&[OrderByClause]>) -> Vec<OrderByClause> {
    let mut clauses = orderby.map(<[_]>::to_vec).unwrap_or_default();
    if clauses
        .last()
        .map(|clause| clause.property != ID_PROPERTY)
        .unwrap_or(true)
    {
        clauses.push(OrderByClause {
            property: ID_PROPERTY.to_string(),
            direction: OrderDirection::Asc,
        });
    }
    clauses
}

/// Build the cursor for a boundary row (the last row of a returned page) under
/// the given canonical ordering.
pub(super) fn cursor_for_row(row: &serde_json::Value, order: &[OrderByClause]) -> Cursor {
    let keys = order
        .iter()
        .map(|clause| match resolve_property(row, &clause.property) {
            Some(value) => CursorKey {
                present: true,
                value,
            },
            None => CursorKey {
                present: false,
                value: serde_json::Value::Null,
            },
        })
        .collect();
    Cursor { keys }
}

/// Encode a cursor as an opaque URL-safe `$skiptoken`.
pub(super) fn encode_cursor(cursor: &Cursor) -> String {
    let array = serde_json::Value::Array(
        cursor
            .keys
            .iter()
            .map(|key| {
                serde_json::Value::Array(vec![
                    serde_json::Value::Bool(key.present),
                    key.value.clone(),
                ])
            })
            .collect(),
    );
    // Serializing a Vec of JSON arrays is infallible.
    let bytes = serde_json::to_vec(&array).unwrap_or_default();
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Decode a `$skiptoken` back into a cursor.
///
/// Returns `None` for a malformed token (bad base64, bad JSON, or the wrong
/// shape) — a client error the caller surfaces as a 400 rather than silently
/// restarting pagination.
pub(super) fn decode_cursor(token: &str) -> Option<Cursor> {
    let bytes = URL_SAFE_NO_PAD.decode(token.as_bytes()).ok()?;
    let array = serde_json::from_slice::<serde_json::Value>(&bytes).ok()?;
    let entries = array.as_array()?;
    let mut keys = Vec::with_capacity(entries.len());
    for entry in entries {
        let pair = entry.as_array()?;
        if pair.len() != 2 {
            return None;
        }
        let present = pair[0].as_bool()?;
        keys.push(CursorKey {
            present,
            value: pair[1].clone(),
        });
    }
    if keys.is_empty() {
        return None;
    }
    Some(Cursor { keys })
}

/// Build the `$filter` predicate selecting rows that sort strictly after the
/// cursor under `order`.
///
/// Returns `None` when the cursor does not line up with the ordering (wrong
/// number of keys, or a non-scalar boundary value) — the caller treats that as a
/// malformed token.
pub(super) fn keyset_after_predicate(
    cursor: &Cursor,
    order: &[OrderByClause],
) -> Option<FilterExpr> {
    if cursor.keys.len() != order.len() {
        return None;
    }
    keyset_from(cursor, order, 0)
}

fn keyset_from(cursor: &Cursor, order: &[OrderByClause], index: usize) -> Option<FilterExpr> {
    let Some(clause) = order.get(index) else {
        // Past the last (unique) key: two rows equal on every key are the same
        // row, which is not strictly after itself.
        return Some(literal_false());
    };
    let key = &cursor.keys[index];
    let strictly_after = strictly_after_at(clause, key)?;
    let equal_here = equal_at(clause, key);
    let deeper = keyset_from(cursor, order, index + 1)?;
    // strictly_after OR (equal_here AND after(next))
    Some(or(strictly_after, and(equal_here, deeper)))
}

/// The predicate "this clause's value sorts strictly after the boundary value",
/// honoring NULLS LAST for ascending and NULLS FIRST for descending.
fn strictly_after_at(clause: &OrderByClause, key: &CursorKey) -> Option<FilterExpr> {
    let prop = FilterExpr::Property(clause.property.clone());
    match (clause.direction, key.present) {
        // Ascending, boundary has a value: a larger value, or a null (nulls last).
        (OrderDirection::Asc, true) => {
            let literal = odata_literal(&key.value)?;
            Some(or(
                compare(prop.clone(), BinaryOperator::Gt, literal),
                is_null(prop),
            ))
        }
        // Ascending, boundary is null (sorts at the very end): nothing is after it.
        (OrderDirection::Asc, false) => Some(literal_false()),
        // Descending, boundary has a value: a smaller value (nulls sort first, so
        // a null is before — not after — a non-null boundary).
        (OrderDirection::Desc, true) => {
            let literal = odata_literal(&key.value)?;
            Some(compare(prop, BinaryOperator::Lt, literal))
        }
        // Descending, boundary is null (sorts at the very start): any value is after.
        (OrderDirection::Desc, false) => Some(is_not_null(prop)),
    }
}

/// The predicate "this clause's value equals the boundary value" (a null
/// boundary matches a null/absent value), used to descend to the next key.
fn equal_at(clause: &OrderByClause, key: &CursorKey) -> FilterExpr {
    let prop = FilterExpr::Property(clause.property.clone());
    if key.present {
        match odata_literal(&key.value) {
            Some(literal) => compare(prop, BinaryOperator::Eq, literal),
            None => literal_false(),
        }
    } else {
        is_null(prop)
    }
}

fn odata_literal(value: &serde_json::Value) -> Option<FilterExpr> {
    let literal = match value {
        serde_json::Value::Null => ODataValue::Null,
        serde_json::Value::Bool(b) => ODataValue::Boolean(*b),
        serde_json::Value::String(s) => ODataValue::String(s.clone()),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                ODataValue::Int(i)
            } else {
                ODataValue::Float(n.as_f64()?)
            }
        }
        // Arrays/objects are not orderable scalars; a cursor should never hold one.
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => return None,
    };
    Some(FilterExpr::Literal(literal))
}

fn compare(prop: FilterExpr, op: BinaryOperator, literal: FilterExpr) -> FilterExpr {
    FilterExpr::BinaryOp {
        left: Box::new(prop),
        op,
        right: Box::new(literal),
    }
}

fn is_null(prop: FilterExpr) -> FilterExpr {
    compare(
        prop,
        BinaryOperator::Eq,
        FilterExpr::Literal(ODataValue::Null),
    )
}

fn is_not_null(prop: FilterExpr) -> FilterExpr {
    compare(
        prop,
        BinaryOperator::Ne,
        FilterExpr::Literal(ODataValue::Null),
    )
}

fn and(left: FilterExpr, right: FilterExpr) -> FilterExpr {
    FilterExpr::BinaryOp {
        left: Box::new(left),
        op: BinaryOperator::And,
        right: Box::new(right),
    }
}

fn or(left: FilterExpr, right: FilterExpr) -> FilterExpr {
    FilterExpr::BinaryOp {
        left: Box::new(left),
        op: BinaryOperator::Or,
        right: Box::new(right),
    }
}

/// A predicate that is always false, without referencing any property.
/// `not (true)` — the in-memory evaluator and the SQL translator both fold it.
fn literal_false() -> FilterExpr {
    FilterExpr::UnaryOp {
        op: temper_odata::query::types::UnaryOperator::Not,
        operand: Box::new(FilterExpr::Literal(ODataValue::Boolean(true))),
    }
}

/// Combine a caller's optional `$filter` with the keyset predicate.
pub(super) fn and_filter(base: Option<FilterExpr>, keyset: FilterExpr) -> FilterExpr {
    match base {
        Some(base) => and(base, keyset),
        None => keyset,
    }
}

#[cfg(test)]
mod tests;
