//! OData `$filter` → SQL WHERE clause translator for the query projection.
//!
//! Translates a parsed [`FilterExpr`] AST into a SQL WHERE clause that can be
//! executed against the query projection. This enables filter push-down:
//! instead of materializing every entity actor and filtering in memory, we query
//! the projection to get only the matching entity IDs.
//!
//! Ordinary fields are matched through the `entity_field_index` EAV table.
//! `Status` is the canonical entity lifecycle column, so status predicates bind
//! directly to the projection row's `status` column.
//!
//! The translator returns `None` for expressions it cannot handle, signalling
//! the caller to fall back to the existing in-memory filter path.

use temper_odata::query::types::{BinaryOperator, FilterExpr, ODataValue, UnaryOperator};

const MAX_INDEXABLE_FIELD_VALUE_BYTES: usize = 2000;

/// Result of a successful filter translation.
pub(super) struct TranslatedFilter {
    /// SQL WHERE clause fragment (parameters referenced as `?N` starting at `?3`).
    pub where_clause: String,
    /// Ordered parameter values corresponding to `?3`, `?4`, etc.
    pub params: Vec<String>,
}

/// Try to translate an OData filter expression into a SQL WHERE clause.
///
/// Returns `None` if the expression contains constructs that cannot be
/// translated (cross-field comparisons, unsupported functions, etc.).
/// The caller should fall back to in-memory filtering in that case.
#[cfg(test)]
pub(super) fn try_translate_filter(filter: &FilterExpr) -> Option<TranslatedFilter> {
    let mut ctx = TranslateCtx {
        params: Vec::new(),
        next_param: 3, // ?1 = tenant, ?2 = entity_type (prepended by query_field_index)
    };
    let clause = translate_expr(filter, &mut ctx)?;
    Some(TranslatedFilter {
        where_clause: clause,
        params: ctx.params,
    })
}

/// Try to translate a filter into a lossless candidate predicate.
///
/// The durable field index stores scalar values as text and skips oversized
/// strings, so this is intentionally more conservative than
/// [`try_translate_filter`]. Callers that need exact OData behavior can use
/// this predicate as a candidate narrowing step and still re-evaluate the
/// original filter after materialization.
pub(super) fn try_translate_candidate_filter(filter: &FilterExpr) -> Option<TranslatedFilter> {
    let mut ctx = TranslateCtx {
        params: Vec::new(),
        next_param: 3, // ?1 = tenant, ?2 = entity_type (prepended by query_field_index)
    };
    let clause = translate_candidate_expr(filter, &mut ctx)?;
    Some(TranslatedFilter {
        where_clause: clause,
        params: ctx.params,
    })
}

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
pub(super) fn equality_field_predicates(filter: &FilterExpr) -> Option<Vec<(String, String)>> {
    let mut pairs = Vec::new();
    collect_equality_field_predicates(filter, &mut pairs).then_some(pairs)
}

fn collect_equality_field_predicates(expr: &FilterExpr, out: &mut Vec<(String, String)>) -> bool {
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
            let rendered = match value {
                ODataValue::Null => String::new(),
                other => match odata_value_to_text(other) {
                    Some(text) => text,
                    None => return false,
                },
            };
            out.push((field_name.to_string(), rendered));
            true
        }
        _ => false,
    }
}

struct TranslateCtx {
    params: Vec<String>,
    next_param: usize,
}

impl TranslateCtx {
    fn add_param(&mut self, value: String) -> String {
        let placeholder = format!("?{}", self.next_param);
        self.next_param += 1;
        self.params.push(value);
        placeholder
    }
}

/// Recursively translate a FilterExpr into a SQL fragment.
fn translate_expr(expr: &FilterExpr, ctx: &mut TranslateCtx) -> Option<String> {
    match expr {
        FilterExpr::BinaryOp { left, op, right } => {
            match op {
                // Boolean combinators
                BinaryOperator::And => {
                    let l = translate_expr(left, ctx)?;
                    let r = translate_expr(right, ctx)?;
                    Some(format!("({l} AND {r})"))
                }
                BinaryOperator::Or => {
                    let l = translate_expr(left, ctx)?;
                    let r = translate_expr(right, ctx)?;
                    Some(format!("({l} OR {r})"))
                }

                // Comparison operators: Property op Literal
                BinaryOperator::Eq
                | BinaryOperator::Ne
                | BinaryOperator::Gt
                | BinaryOperator::Ge
                | BinaryOperator::Lt
                | BinaryOperator::Le => translate_comparison(left, *op, right, ctx),

                // `has` (enum flag) — not supported for SQL push-down
                BinaryOperator::Has => None,
            }
        }

        FilterExpr::UnaryOp { op, operand } => match op {
            UnaryOperator::Not => {
                let inner = translate_expr(operand, ctx)?;
                Some(format!("(NOT {inner})"))
            }
        },

        FilterExpr::FunctionCall { name, args } => translate_function(name, args, ctx),

        // Bare property or literal at top level — not a valid boolean filter
        FilterExpr::Property(_) | FilterExpr::Literal(_) => None,
    }
}

/// Translate the lossless candidate portion of a filter.
///
/// The query-plane candidate SQL is a narrowing step, not the final OData
/// filter. For `AND`, lossless conjuncts can be pushed independently while
/// non-lossless conjuncts are still re-checked after materialization. For
/// `OR`, dropping one side would change the candidate set, so the whole
/// expression must be lossless or it is not pushed.
fn translate_candidate_expr(expr: &FilterExpr, ctx: &mut TranslateCtx) -> Option<String> {
    match expr {
        FilterExpr::BinaryOp { left, op, right } => match op {
            BinaryOperator::And => {
                let left_clause = translate_candidate_expr(left, ctx);
                let right_clause = translate_candidate_expr(right, ctx);
                match (left_clause, right_clause) {
                    (Some(left_clause), Some(right_clause)) => {
                        Some(format!("({left_clause} AND {right_clause})"))
                    }
                    (Some(clause), None) | (None, Some(clause)) => Some(clause),
                    (None, None) => None,
                }
            }
            BinaryOperator::Or => {
                if candidate_filter_is_lossless(expr) {
                    translate_expr(expr, ctx)
                } else {
                    None
                }
            }
            BinaryOperator::Eq
            | BinaryOperator::Ne
            | BinaryOperator::Gt
            | BinaryOperator::Ge
            | BinaryOperator::Lt
            | BinaryOperator::Le
            | BinaryOperator::Has => {
                if candidate_filter_is_lossless(expr) {
                    translate_expr(expr, ctx)
                } else {
                    None
                }
            }
        },
        FilterExpr::UnaryOp { .. }
        | FilterExpr::FunctionCall { .. }
        | FilterExpr::Property(_)
        | FilterExpr::Literal(_) => {
            if candidate_filter_is_lossless(expr) {
                translate_expr(expr, ctx)
            } else {
                None
            }
        }
    }
}

/// Translate `Property op Literal` into a subquery against the EAV table.
///
/// The EAV pattern means each property comparison becomes:
/// ```sql
/// entity_id IN (
///   SELECT entity_id FROM entity_field_index
///   WHERE tenant = ?1 AND entity_type = ?2
///     AND field_name = ?N AND field_value <op> ?M
/// )
/// ```
fn translate_comparison(
    left: &FilterExpr,
    op: BinaryOperator,
    right: &FilterExpr,
    ctx: &mut TranslateCtx,
) -> Option<String> {
    // Extract property name and literal value (either direction)
    let (field_name, value) = match (left, right) {
        (FilterExpr::Property(name), FilterExpr::Literal(val)) => (name.clone(), val),
        (FilterExpr::Literal(val), FilterExpr::Property(name)) => (name.clone(), val),
        _ => return None, // cross-field or complex expressions
    };

    let sql_op = match op {
        BinaryOperator::Eq => "=",
        BinaryOperator::Ne => "!=",
        BinaryOperator::Gt => ">",
        BinaryOperator::Ge => ">=",
        BinaryOperator::Lt => "<",
        BinaryOperator::Le => "<=",
        _ => return None,
    };

    if is_status_property(&field_name) {
        return translate_status_comparison(sql_op, op, value, ctx);
    }

    // Handle null comparison specially
    if matches!(value, ODataValue::Null) {
        let field_param = ctx.add_param(field_name);
        return match op {
            BinaryOperator::Eq => Some(format!(
                "entity_id IN (SELECT entity_id FROM entity_field_index \
                 WHERE tenant = ?1 AND entity_type = ?2 \
                 AND field_name = {field_param} AND field_value IS NULL)"
            )),
            BinaryOperator::Ne => Some(format!(
                "entity_id IN (SELECT entity_id FROM entity_field_index \
                 WHERE tenant = ?1 AND entity_type = ?2 \
                 AND field_name = {field_param} AND field_value IS NOT NULL)"
            )),
            _ => None, // gt/lt/ge/le with null doesn't make sense
        };
    }

    let text_value = odata_value_to_text(value)?;
    let field_param = ctx.add_param(field_name);
    let value_param = ctx.add_param(text_value);

    Some(format!(
        "entity_id IN (SELECT entity_id FROM entity_field_index \
         WHERE tenant = ?1 AND entity_type = ?2 \
         AND field_name = {field_param} AND field_value {sql_op} {value_param})"
    ))
}

fn is_status_property(field_name: &str) -> bool {
    field_name == "Status" || field_name == "status"
}

fn candidate_filter_is_lossless(expr: &FilterExpr) -> bool {
    match expr {
        FilterExpr::BinaryOp { left, op, right } => match op {
            BinaryOperator::And | BinaryOperator::Or => {
                candidate_filter_is_lossless(left) && candidate_filter_is_lossless(right)
            }
            BinaryOperator::Eq => lossless_eq_comparison(left, right),
            BinaryOperator::Gt | BinaryOperator::Ge | BinaryOperator::Lt | BinaryOperator::Le => {
                status_literal_comparison(left, right)
            }
            BinaryOperator::Ne | BinaryOperator::Has => false,
        },
        FilterExpr::UnaryOp { .. } | FilterExpr::FunctionCall { .. } => false,
        FilterExpr::Property(_) | FilterExpr::Literal(_) => false,
    }
}

fn lossless_eq_comparison(left: &FilterExpr, right: &FilterExpr) -> bool {
    let Some((field_name, value)) = property_literal_pair(left, right) else {
        return false;
    };
    if is_status_property(field_name) {
        return matches!(value, ODataValue::Null | ODataValue::String(_));
    }
    match value {
        // Ordinary JSON nulls are not a lossless candidate predicate today:
        // Postgres skips null field-index rows, so null equality must be
        // proven after materialization unless a backend advertises null indexes.
        ODataValue::Null => false,
        ODataValue::Boolean(_) => true,
        ODataValue::String(value) => value.len() <= MAX_INDEXABLE_FIELD_VALUE_BYTES,
        ODataValue::Int(_)
        | ODataValue::Float(_)
        | ODataValue::Guid(_)
        | ODataValue::DateTimeOffset(_) => false,
    }
}

fn status_literal_comparison(left: &FilterExpr, right: &FilterExpr) -> bool {
    matches!(
        (left, right),
        (FilterExpr::Property(field_name), FilterExpr::Literal(ODataValue::String(_)))
            if is_status_property(field_name)
    )
}

fn property_literal_pair<'a>(
    left: &'a FilterExpr,
    right: &'a FilterExpr,
) -> Option<(&'a str, &'a ODataValue)> {
    match (left, right) {
        (FilterExpr::Property(name), FilterExpr::Literal(value))
        | (FilterExpr::Literal(value), FilterExpr::Property(name)) => Some((name.as_str(), value)),
        _ => None,
    }
}

fn translate_status_comparison(
    sql_op: &str,
    op: BinaryOperator,
    value: &ODataValue,
    ctx: &mut TranslateCtx,
) -> Option<String> {
    if matches!(value, ODataValue::Null) {
        return match op {
            BinaryOperator::Eq => Some("status IS NULL".to_string()),
            BinaryOperator::Ne => Some("status IS NOT NULL".to_string()),
            _ => None,
        };
    }

    let text_value = odata_value_to_text(value)?;
    let value_param = ctx.add_param(text_value);
    Some(format!("status {sql_op} {value_param}"))
}

/// Translate OData string functions to SQL LIKE patterns.
fn translate_function(name: &str, args: &[FilterExpr], ctx: &mut TranslateCtx) -> Option<String> {
    match name {
        "contains" | "startswith" | "endswith" => {
            if args.len() != 2 {
                return None;
            }
            let field_name = match &args[0] {
                FilterExpr::Property(name) => name.clone(),
                _ => return None,
            };
            let search_value = match &args[1] {
                FilterExpr::Literal(ODataValue::String(s)) => s.clone(),
                _ => return None,
            };

            // Escape SQL LIKE wildcards in the search value
            let escaped = search_value.replace('%', "\\%").replace('_', "\\_");

            let pattern = match name {
                "contains" => format!("%{escaped}%"),
                "startswith" => format!("{escaped}%"),
                "endswith" => format!("%{escaped}"),
                _ => unreachable!(),
            };

            let field_param = ctx.add_param(field_name);
            let pattern_param = ctx.add_param(pattern);

            Some(format!(
                "entity_id IN (SELECT entity_id FROM entity_field_index \
                 WHERE tenant = ?1 AND entity_type = ?2 \
                 AND field_name = {field_param} AND field_value LIKE {pattern_param} ESCAPE '\\')"
            ))
        }
        _ => None, // unsupported function → fallback
    }
}

/// Convert an ODataValue to its TEXT representation for the index.
fn odata_value_to_text(value: &ODataValue) -> Option<String> {
    match value {
        ODataValue::Null => None, // handled separately
        ODataValue::Boolean(b) => Some(b.to_string()),
        ODataValue::Int(i) => Some(i.to_string()),
        ODataValue::Float(f) => Some(f.to_string()),
        ODataValue::String(s) => Some(s.clone()),
        ODataValue::Guid(g) => Some(g.to_string()),
        ODataValue::DateTimeOffset(dt) => Some(dt.to_rfc3339()),
    }
}

#[cfg(test)]
mod tests;
