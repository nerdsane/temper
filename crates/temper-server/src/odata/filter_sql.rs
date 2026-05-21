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
mod tests {
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
}
