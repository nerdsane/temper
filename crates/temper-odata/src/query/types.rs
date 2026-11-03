//! OData v4 query options types and top-level parser.
//!
//! Parses `$filter`, `$select`, `$expand`, `$orderby`, `$top`, `$skip`, and `$count`
//! from a URL query string, or from decoded pairs, into [`QueryOptions`].

use super::filter::parse_filter;
use super::query_string::{split_key_value, split_query_string};
use crate::error::ODataError;

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
    /// The `$skiptoken` server-driven-paging continuation token, if present.
    ///
    /// Opaque to clients: the server issues it inside an `@odata.nextLink` and
    /// interprets it as a keyset cursor over the response's ordering. Clients
    /// echo it back verbatim; they never construct one.
    pub skiptoken: Option<String>,
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
    FunctionCall { name: String, args: Vec<FilterExpr> },
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

/// Parse a raw URL query string (without leading `?`) into [`QueryOptions`].
///
/// The string is split into parameters before each key and value is
/// percent-decoded, so an encoded `&` or `=` stays inside its value. Callers
/// holding decoded pairs, such as an HTTP query extractor, use
/// [`parse_query_options_from_pairs`].
///
/// Example: `$filter=Name%20eq%20'foo'&$top=10&$select=Id,Name`
pub fn parse_query_options(query_string: &str) -> Result<QueryOptions, ODataError> {
    let pairs = split_query_string(query_string.trim())?;
    parse_query_options_from_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())))
}

/// Parse separated, already-decoded query parameters into [`QueryOptions`].
///
/// Each value reaches its option's parser exactly as given: nothing is joined,
/// split or decoded again, so `&`, `=` and parentheses inside a `$filter`
/// literal are data. Keys are trimmed; parameters that are not system query
/// options are ignored.
pub fn parse_query_options_from_pairs<'a>(
    pairs: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<QueryOptions, ODataError> {
    let mut opts = QueryOptions::default();

    for (key, value) in pairs {
        match key.trim() {
            "$filter" => {
                opts.filter = Some(parse_filter(value)?);
            }
            "$select" => {
                opts.select = Some(parse_select(value));
            }
            "$expand" => {
                opts.expand = Some(parse_expand(value, 1)?);
            }
            "$orderby" => {
                opts.orderby = Some(parse_orderby(value)?);
            }
            "$top" => {
                opts.top = Some(parse_usize("$top", value)?);
            }
            "$skip" => {
                opts.skip = Some(parse_usize("$skip", value)?);
            }
            "$count" => {
                opts.count = Some(parse_bool("$count", value)?);
            }
            "$skiptoken" => {
                let token = value.trim();
                if !token.is_empty() {
                    opts.skiptoken = Some(token.to_string());
                }
            }
            // Custom options, $format and system options not handled yet are ignored.
            _ => {}
        }
    }

    Ok(opts)
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
                            message: format!(
                                "invalid direction '{other}', expected 'asc' or 'desc'"
                            ),
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

/// The most levels one `$expand` may nest (`A($expand=B($expand=C))` is three).
///
/// Forced by the memory one read may take. Each level copies every related
/// entity into the response, so a level can multiply the copies by its
/// relation's fan-out, and it is the query string, not the data, that picks
/// how many times. A relation that leads back
/// (`Orders($expand=Customer($expand=Orders(...)))`) can be multiplied as
/// often as the caller likes, so without a bound one short query from any
/// reader could build more copies than the server has memory for and take it
/// down for every tenant.
///
/// Measured 23 Oct 2026 through the server's router (release build), reading
/// `Customers?$expand=Orders($expand=Customer($expand=Orders(...)))`:
///
/// | customers x orders each | depth | entity copies | peak heap | response |
/// |---|---|---|---|---|
/// | 10 x 10  | 3 | 1,210  | 8.9 MB  | 0.5 MB |
/// | 10 x 10  | 4 | 2,210  | 15.2 MB | 0.9 MB |
/// | 10 x 10  | 5 | 12,210 | 86 MB   | 5.0 MB |
/// | 10 x 10  | 6 | 22,210 | 158 MB  | 9.2 MB |
/// | 10 x 30  | 4 | 18,610 | 127 MB  | 7.8 MB |
/// | 100 x 10 | 3 | 12,100 | 85 MB   | 5.0 MB |
///
/// Every run cost 6.7 to 7.2 KiB of heap and about 415 bytes of response per
/// copy, and the copies matched `page x (1 + fan-out + ...)` exactly. So
/// over the default page of 100 entities, with every level a fan-out of 10,
/// three levels build 111,100 copies, about 760 MiB, near all the memory a
/// production Temper process holds at rest (about 0.9 GB). A fourth level
/// would multiply that by ten, to about 7.4 GiB. Three is the deepest level
/// that read fits in.
///
/// The bound limits what the query adds, not what the data holds: one
/// relation with 10,000 targets still costs about 70 MB per parent at depth 1.
///
/// A deeper `$expand` is rejected with a 400 that names this bound; nothing is
/// cut off silently.
pub const MAX_EXPAND_DEPTH: usize = 3;

/// Parse the `$expand` at nesting `level` (the top-level `$expand` is 1).
fn parse_expand(value: &str, level: usize) -> Result<Vec<ExpandItem>, ODataError> {
    if level > MAX_EXPAND_DEPTH {
        return Err(ODataError::InvalidQueryOption {
            option: "$expand".into(),
            message: format!(
                "$expand nests more than {MAX_EXPAND_DEPTH} levels, the most one read may follow"
            ),
        });
    }
    let items = split_expand_items(value);
    let mut result = Vec::new();
    for item in items {
        result.push(parse_expand_item(item.trim(), level)?);
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

fn parse_expand_item(item: &str, level: usize) -> Result<ExpandItem, ODataError> {
    if let Some(paren_start) = item.find('(') {
        if !item.ends_with(')') {
            return Err(ODataError::InvalidQueryOption {
                option: "$expand".into(),
                message: format!("unmatched parenthesis in expand item '{item}'"),
            });
        }
        let property = item[..paren_start].trim().to_string();
        let nested_str = &item[paren_start + 1..item.len() - 1];
        let options = parse_expand_options(nested_str, level)?;
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
fn parse_expand_options(s: &str, level: usize) -> Result<ExpandOptions, ODataError> {
    let mut opts = ExpandOptions::default();

    // Split by semicolons, respecting parentheses.
    let parts = split_by_semicolon(s);

    for part in parts {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (key, value) = split_key_value(part)?;
        match key {
            "$select" => {
                opts.select = Some(parse_select(value));
            }
            "$filter" => {
                opts.filter = Some(parse_filter(value)?);
            }
            "$orderby" => {
                opts.orderby = Some(parse_orderby(value)?);
            }
            "$top" => {
                opts.top = Some(parse_usize("$top", value)?);
            }
            "$skip" => {
                opts.skip = Some(parse_usize("$skip", value)?);
            }
            "$expand" => {
                opts.expand = Some(parse_expand(value, level + 1)?);
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
    value
        .trim()
        .parse::<usize>()
        .map_err(|_| ODataError::InvalidQueryOption {
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

#[cfg(test)]
#[path = "expand_depth_test.rs"]
mod expand_depth_tests;

#[cfg(test)]
#[path = "query_options_test.rs"]
mod query_options_tests;

#[cfg(test)]
#[path = "types_test.rs"]
mod tests;
