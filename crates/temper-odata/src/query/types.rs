//! OData v4 query options types and top-level parser.
//!
//! Parses `$filter`, `$select`, `$expand`, `$orderby`, `$top`, `$skip`, and `$count`
//! from a URL query string into a structured [`QueryOptions`].

use crate::error::ODataError;
use super::filter::parse_filter;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// All parsed OData system query options.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryOptions {
    /// The `$filter` expression, if present.
    pub filter: Option<FilterExpr>,
    /// The `$select` property list, if present.
    pub select: Option<Vec<String>>,
    /// The `$expand` items, if present.
    pub expand: Option<Vec<ExpandItem>>,
    /// The `$orderby` clauses, if present.
    pub orderby: Option<Vec<OrderByClause>>,
    /// The `$top` limit, if present.
    pub top: Option<usize>,
    /// The `$skip` offset, if present.
    pub skip: Option<usize>,
    /// The `$count` flag, if present.
    pub count: Option<bool>,
}

/// An item in a `$expand` clause, optionally with nested query options.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpandItem {
    /// The navigation property name to expand.
    pub property: String,
    /// Nested query options for this expand item.
    pub options: Option<ExpandOptions>,
}

/// Nested query options inside an `$expand` item.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ExpandOptions {
    /// Nested `$select` property list.
    pub select: Option<Vec<String>>,
    /// Nested `$filter` expression.
    pub filter: Option<FilterExpr>,
    /// Nested `$orderby` clauses.
    pub orderby: Option<Vec<OrderByClause>>,
    /// Nested `$top` limit.
    pub top: Option<usize>,
    /// Nested `$skip` offset.
    pub skip: Option<usize>,
    /// Nested `$expand` items.
    pub expand: Option<Vec<ExpandItem>>,
}

/// A single clause in `$orderby`, e.g. `CreatedAt desc`.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderByClause {
    /// The property name to sort by.
    pub property: String,
    /// The sort direction (ascending or descending).
    pub direction: OrderDirection,
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDirection {
    /// Ascending order (default).
    Asc,
    /// Descending order.
    Desc,
}

/// A filter expression AST node.
#[derive(Debug, Clone, PartialEq)]
pub enum FilterExpr {
    /// A binary operation, e.g. `Name eq 'foo'` or `left and right`.
    BinaryOp {
        left: Box<FilterExpr>,
        op: BinaryOperator,
        right: Box<FilterExpr>,
    },
    /// A unary operation, e.g. `not expr`.
    UnaryOp {
        op: UnaryOperator,
        operand: Box<FilterExpr>,
    },
    /// A property path, e.g. `Name` or `Address/City`.
    Property(String),
    /// A literal value.
    Literal(ODataValue),
    /// A function call, e.g. `contains(Name, 'foo')`.
    FunctionCall {
        name: String,
        args: Vec<FilterExpr>,
    },
}

/// Binary operators in `$filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOperator {
    /// Equal.
    Eq,
    /// Not equal.
    Ne,
    /// Greater than.
    Gt,
    /// Greater than or equal.
    Ge,
    /// Less than.
    Lt,
    /// Less than or equal.
    Le,
    /// Logical AND.
    And,
    /// Logical OR.
    Or,
    /// Enum flag check.
    Has,
}

/// Unary operators in `$filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    /// Logical negation.
    Not,
}

/// A literal value in an OData expression.
#[derive(Debug, Clone, PartialEq)]
pub enum ODataValue {
    /// The null literal.
    Null,
    /// A boolean literal (`true` or `false`).
    Boolean(bool),
    /// A 64-bit integer literal.
    Int(i64),
    /// A 64-bit floating-point literal.
    Float(f64),
    /// A string literal (single-quoted in OData).
    String(String),
    /// A GUID literal.
    Guid(uuid::Uuid),
    /// A DateTimeOffset literal.
    DateTimeOffset(chrono::DateTime<chrono::Utc>),
}

// ---------------------------------------------------------------------------
// Top-level query string parser
// ---------------------------------------------------------------------------

/// Parse a URL query string (without leading `?`) into [`QueryOptions`].
///
/// Example: `$filter=Name eq 'foo'&$top=10&$select=Id,Name`
pub fn parse_query_options(query_string: &str) -> Result<QueryOptions, ODataError> {
    let query_string = query_string.trim();
    if query_string.is_empty() {
        return Ok(QueryOptions::default());
    }

    let mut opts = QueryOptions::default();

    for pair in split_query_params(query_string) {
        let (key, value) = split_key_value(&pair)?;
        match key.as_str() {
            "$filter" => {
                opts.filter = Some(parse_filter(&value)?);
            }
            "$select" => {
                opts.select = Some(parse_select(&value));
            }
            "$expand" => {
                opts.expand = Some(parse_expand(&value)?);
            }
            "$orderby" => {
                opts.orderby = Some(parse_orderby(&value)?);
            }
            "$top" => {
                opts.top = Some(parse_usize("$top", &value)?);
            }
            "$skip" => {
                opts.skip = Some(parse_usize("$skip", &value)?);
            }
            "$count" => {
                opts.count = Some(parse_bool("$count", &value)?);
            }
            other => {
                // Ignore non-system query options (custom options, $format, etc.)
                if other.starts_with('$') {
                    // Known system options we don't handle yet -- soft ignore.
                    // For strict mode we could return UnsupportedOption.
                }
            }
        }
    }

    Ok(opts)
}

/// Split query string parameters, respecting parentheses (for nested $expand).
fn split_query_params(qs: &str) -> Vec<String> {
    let mut params = Vec::new();
    let mut current = String::new();
    let mut paren_depth: u32 = 0;

    for ch in qs.chars() {
        match ch {
            '(' => {
                paren_depth += 1;
                current.push(ch);
            }
            ')' => {
                paren_depth = paren_depth.saturating_sub(1);
                current.push(ch);
            }
            '&' if paren_depth == 0 => {
                if !current.is_empty() {
                    params.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        params.push(current);
    }
    params
}

fn split_key_value(pair: &str) -> Result<(String, String), ODataError> {
    if let Some(eq_pos) = pair.find('=') {
        let key = pair[..eq_pos].trim().to_string();
        let value = pair[eq_pos + 1..].to_string();
        Ok((key, value))
    } else {
        Err(ODataError::InvalidQueryOption {
            option: pair.to_string(),
            message: "missing '=' in query parameter".into(),
        })
    }
}

// ---------------------------------------------------------------------------
// $select parser
// ---------------------------------------------------------------------------

fn parse_select(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

// ---------------------------------------------------------------------------
// $orderby parser
// ---------------------------------------------------------------------------

fn parse_orderby(value: &str) -> Result<Vec<OrderByClause>, ODataError> {
    let mut clauses = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let tokens: Vec<&str> = part.split_whitespace().collect();
        match tokens.len() {
            1 => {
                clauses.push(OrderByClause {
                    property: tokens[0].to_string(),
                    direction: OrderDirection::Asc,
                });
            }
            2 => {
                let direction = match tokens[1].to_ascii_lowercase().as_str() {
                    "asc" => OrderDirection::Asc,
                    "desc" => OrderDirection::Desc,
                    other => {
                        return Err(ODataError::InvalidQueryOption {
                            option: "$orderby".into(),
                            message: format!("invalid direction '{other}', expected 'asc' or 'desc'"),
                        });
                    }
                };
                clauses.push(OrderByClause {
                    property: tokens[0].to_string(),
                    direction,
                });
            }
            _ => {
                return Err(ODataError::InvalidQueryOption {
                    option: "$orderby".into(),
                    message: format!("invalid orderby clause '{part}'"),
                });
            }
        }
    }
    Ok(clauses)
}

// ---------------------------------------------------------------------------
// $expand parser
// ---------------------------------------------------------------------------

fn parse_expand(value: &str) -> Result<Vec<ExpandItem>, ODataError> {
    let items = split_expand_items(value);
    let mut result = Vec::new();
    for item in items {
        result.push(parse_expand_item(item.trim())?);
    }
    Ok(result)
}

/// Split expand items by comma at the top level (not inside parens).
fn split_expand_items(s: &str) -> Vec<String> {
    let mut items = Vec::new();
    let mut current = String::new();
    let mut depth: u32 = 0;

    for ch in s.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                if !current.is_empty() {
                    items.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        items.push(current);
    }
    items
}

fn parse_expand_item(item: &str) -> Result<ExpandItem, ODataError> {
    if let Some(paren_start) = item.find('(') {
        if !item.ends_with(')') {
            return Err(ODataError::InvalidQueryOption {
                option: "$expand".into(),
                message: format!("unmatched parenthesis in expand item '{item}'"),
            });
        }
        let property = item[..paren_start].trim().to_string();
        let nested_str = &item[paren_start + 1..item.len() - 1];
        let options = parse_expand_options(nested_str)?;
        Ok(ExpandItem {
            property,
            options: Some(options),
        })
    } else {
        Ok(ExpandItem {
            property: item.trim().to_string(),
            options: None,
        })
    }
}

/// Parse the options inside an expand item's parentheses.
///
/// These are semicolon-separated, e.g. `$select=Id,Quantity;$orderby=Quantity desc`.
fn parse_expand_options(s: &str) -> Result<ExpandOptions, ODataError> {
    let mut opts = ExpandOptions::default();

    // Split by semicolons, respecting parentheses.
    let parts = split_by_semicolon(s);

    for part in parts {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (key, value) = split_key_value(part)?;
        match key.as_str() {
            "$select" => {
                opts.select = Some(parse_select(&value));
            }
            "$filter" => {
                opts.filter = Some(parse_filter(&value)?);
            }
            "$orderby" => {
                opts.orderby = Some(parse_orderby(&value)?);
            }
            "$top" => {
                opts.top = Some(parse_usize("$top", &value)?);
            }
            "$skip" => {
                opts.skip = Some(parse_usize("$skip", &value)?);
            }
            "$expand" => {
                opts.expand = Some(parse_expand(&value)?);
            }
            _ => {
                // Ignore unknown nested options
            }
        }
    }

    Ok(opts)
}

fn split_by_semicolon(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth: u32 = 0;

    for ch in s.chars() {
        match ch {
            '(' => {
                depth += 1;
                current.push(ch);
            }
            ')' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ';' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        parts.push(current);
    }
    parts
}

// ---------------------------------------------------------------------------
// Simple helpers
// ---------------------------------------------------------------------------

fn parse_usize(option: &str, value: &str) -> Result<usize, ODataError> {
    value.trim().parse::<usize>().map_err(|_| ODataError::InvalidQueryOption {
        option: option.into(),
        message: format!("expected a non-negative integer, got '{value}'"),
    })
}

fn parse_bool(option: &str, value: &str) -> Result<bool, ODataError> {
    match value.trim().to_ascii_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(ODataError::InvalidQueryOption {
            option: option.into(),
            message: format!("expected 'true' or 'false', got '{value}'"),
        }),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- $select tests -------------------------------------------------------

    #[test]
    fn select_multiple_properties() {
        let opts = parse_query_options("$select=Id,Name,Total").unwrap();
        assert_eq!(
            opts.select,
            Some(vec!["Id".into(), "Name".into(), "Total".into()])
        );
    }

    // -- $orderby tests ------------------------------------------------------

    #[test]
    fn orderby_multiple_clauses() {
        let opts = parse_query_options("$orderby=CreatedAt desc,Name asc").unwrap();
        assert_eq!(
            opts.orderby,
            Some(vec![
                OrderByClause {
                    property: "CreatedAt".into(),
                    direction: OrderDirection::Desc,
                },
                OrderByClause {
                    property: "Name".into(),
                    direction: OrderDirection::Asc,
                },
            ])
        );
    }

    #[test]
    fn orderby_default_direction_is_asc() {
        let opts = parse_query_options("$orderby=Name").unwrap();
        assert_eq!(
            opts.orderby,
            Some(vec![OrderByClause {
                property: "Name".into(),
                direction: OrderDirection::Asc,
            }])
        );
    }

    // -- $top, $skip, $count tests -------------------------------------------

    #[test]
    fn top_skip_count() {
        let opts =
            parse_query_options("$top=10&$skip=20&$count=true").unwrap();
        assert_eq!(opts.top, Some(10));
        assert_eq!(opts.skip, Some(20));
        assert_eq!(opts.count, Some(true));
    }

    // -- $expand tests -------------------------------------------------------

    #[test]
    fn expand_simple() {
        let opts = parse_query_options("$expand=Items").unwrap();
        assert_eq!(
            opts.expand,
            Some(vec![ExpandItem {
                property: "Items".into(),
                options: None,
            }])
        );
    }

    #[test]
    fn expand_with_nested_options() {
        let opts = parse_query_options(
            "$expand=Items($select=Id,Quantity;$orderby=Quantity desc)",
        )
        .unwrap();
        let expand = opts.expand.unwrap();
        assert_eq!(expand.len(), 1);
        assert_eq!(expand[0].property, "Items");
        let nested = expand[0].options.as_ref().unwrap();
        assert_eq!(
            nested.select,
            Some(vec!["Id".into(), "Quantity".into()])
        );
        assert_eq!(
            nested.orderby,
            Some(vec![OrderByClause {
                property: "Quantity".into(),
                direction: OrderDirection::Desc,
            }])
        );
    }

    #[test]
    fn expand_multiple_items() {
        let opts = parse_query_options("$expand=Items,Customer").unwrap();
        let expand = opts.expand.unwrap();
        assert_eq!(expand.len(), 2);
        assert_eq!(expand[0].property, "Items");
        assert_eq!(expand[1].property, "Customer");
    }

    // -- Combined query test -------------------------------------------------

    #[test]
    fn combined_query_options() {
        let opts = parse_query_options(
            "$filter=Status eq 'Active'&$select=Id,Name&$orderby=Name asc&$top=50&$skip=0&$count=true",
        )
        .unwrap();

        assert!(opts.filter.is_some());
        assert_eq!(
            opts.select,
            Some(vec!["Id".into(), "Name".into()])
        );
        assert_eq!(
            opts.orderby,
            Some(vec![OrderByClause {
                property: "Name".into(),
                direction: OrderDirection::Asc,
            }])
        );
        assert_eq!(opts.top, Some(50));
        assert_eq!(opts.skip, Some(0));
        assert_eq!(opts.count, Some(true));
    }

    #[test]
    fn empty_query_string() {
        let opts = parse_query_options("").unwrap();
        assert_eq!(opts, QueryOptions::default());
    }

    #[test]
    fn invalid_top_value() {
        let result = parse_query_options("$top=abc");
        assert!(result.is_err());
    }
}
