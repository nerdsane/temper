//! OData v4 query options parser.
//!
//! Parses `$filter`, `$select`, `$expand`, `$orderby`, `$top`, `$skip`, and `$count`
//! from a URL query string into a structured [`QueryOptions`].

use crate::error::ODataError;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// All parsed OData system query options.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryOptions {
    pub filter: Option<FilterExpr>,
    pub select: Option<Vec<String>>,
    pub expand: Option<Vec<ExpandItem>>,
    pub orderby: Option<Vec<OrderByClause>>,
    pub top: Option<usize>,
    pub skip: Option<usize>,
    pub count: Option<bool>,
}

/// An item in a `$expand` clause, optionally with nested query options.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpandItem {
    pub property: String,
    pub options: Option<ExpandOptions>,
}

/// Nested query options inside an `$expand` item.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ExpandOptions {
    pub select: Option<Vec<String>>,
    pub filter: Option<FilterExpr>,
    pub orderby: Option<Vec<OrderByClause>>,
    pub top: Option<usize>,
    pub skip: Option<usize>,
    pub expand: Option<Vec<ExpandItem>>,
}

/// A single clause in `$orderby`, e.g. `CreatedAt desc`.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderByClause {
    pub property: String,
    pub direction: OrderDirection,
}

/// Sort direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDirection {
    Asc,
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
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    And,
    Or,
    Has,
}

/// Unary operators in `$filter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOperator {
    Not,
}

/// A literal value in an OData expression.
#[derive(Debug, Clone, PartialEq)]
pub enum ODataValue {
    Null,
    Boolean(bool),
    Int(i64),
    Float(f64),
    String(String),
    Guid(uuid::Uuid),
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
                    // Known system options we don't handle yet — soft ignore.
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
// $filter recursive-descent parser
// ---------------------------------------------------------------------------

/// Parse a `$filter` expression string into a [`FilterExpr`] AST.
pub fn parse_filter(input: &str) -> Result<FilterExpr, ODataError> {
    let tokens = tokenize_filter(input)?;
    let mut parser = FilterParser::new(&tokens);
    let expr = parser.parse_or()?;

    // Make sure we consumed everything
    if parser.pos < parser.tokens.len() {
        return Err(ODataError::InvalidFilter {
            message: format!(
                "unexpected token '{}' after expression",
                parser.tokens[parser.pos].text
            ),
            position: parser.tokens[parser.pos].offset,
        });
    }

    Ok(expr)
}

// -- Tokenizer ---------------------------------------------------------------

#[derive(Debug, Clone)]
struct Token {
    text: String,
    offset: usize,
}

fn tokenize_filter(input: &str) -> Result<Vec<Token>, ODataError> {
    let mut tokens = Vec::new();
    let chars: Vec<char> = input.chars().collect();
    let mut i = 0;

    while i < chars.len() {
        // Skip whitespace
        if chars[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }

        let offset = i;

        // String literal: 'value'
        if chars[i] == '\'' {
            i += 1;
            let mut s = String::new();
            while i < chars.len() {
                if chars[i] == '\'' {
                    // Check for escaped quote ''
                    if i + 1 < chars.len() && chars[i + 1] == '\'' {
                        s.push('\'');
                        i += 2;
                    } else {
                        i += 1;
                        break;
                    }
                } else {
                    s.push(chars[i]);
                    i += 1;
                }
            }
            tokens.push(Token {
                text: format!("'{s}'"),
                offset,
            });
            continue;
        }

        // Parentheses and comma
        if chars[i] == '(' || chars[i] == ')' || chars[i] == ',' {
            tokens.push(Token {
                text: chars[i].to_string(),
                offset,
            });
            i += 1;
            continue;
        }

        // Number (possibly negative, possibly decimal)
        if chars[i].is_ascii_digit()
            || (chars[i] == '-'
                && i + 1 < chars.len()
                && chars[i + 1].is_ascii_digit())
        {
            let mut num = String::new();
            if chars[i] == '-' {
                num.push('-');
                i += 1;
            }
            while i < chars.len() && (chars[i].is_ascii_digit() || chars[i] == '.') {
                num.push(chars[i]);
                i += 1;
            }
            tokens.push(Token { text: num, offset });
            continue;
        }

        // Identifiers and keywords (including dotted names like 'guid', property paths)
        if chars[i].is_ascii_alphabetic() || chars[i] == '_' || chars[i] == '$' {
            let mut word = String::new();
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric()
                    || chars[i] == '_'
                    || chars[i] == '.'
                    || chars[i] == '/'
                    || chars[i] == '-')
            {
                // Stop at '.' if it looks like it's followed by whitespace or end
                // (to not consume e.g. "Name eq" as "Name.eq"). Actually, dots
                // are used in property paths like Address/City and in GUIDs.
                // Let's keep consuming alphanumeric, underscore, dot, slash, and hyphen.
                word.push(chars[i]);
                i += 1;
            }
            tokens.push(Token { text: word, offset });
            continue;
        }

        return Err(ODataError::InvalidFilter {
            message: format!("unexpected character '{}'", chars[i]),
            position: i,
        });
    }

    Ok(tokens)
}

// -- Recursive descent parser ------------------------------------------------

struct FilterParser<'a> {
    tokens: &'a [Token],
    pos: usize,
}

impl<'a> FilterParser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn advance(&mut self) -> Option<&Token> {
        let tok = self.tokens.get(self.pos);
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn current_offset(&self) -> usize {
        self.peek().map(|t| t.offset).unwrap_or(0)
    }

    fn expect_text(&mut self, expected: &str) -> Result<(), ODataError> {
        match self.advance() {
            Some(tok) if tok.text == expected => Ok(()),
            Some(tok) => Err(ODataError::InvalidFilter {
                message: format!("expected '{expected}', found '{}'", tok.text),
                position: tok.offset,
            }),
            None => Err(ODataError::InvalidFilter {
                message: format!("expected '{expected}', found end of input"),
                position: self.current_offset(),
            }),
        }
    }

    // -- Grammar rules (lowest to highest precedence) --

    // or_expr = and_expr ( 'or' and_expr )*
    fn parse_or(&mut self) -> Result<FilterExpr, ODataError> {
        let mut left = self.parse_and()?;
        while self.peek_text_is("or") {
            self.advance();
            let right = self.parse_and()?;
            left = FilterExpr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::Or,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    // and_expr = not_expr ( 'and' not_expr )*
    fn parse_and(&mut self) -> Result<FilterExpr, ODataError> {
        let mut left = self.parse_not()?;
        while self.peek_text_is("and") {
            self.advance();
            let right = self.parse_not()?;
            left = FilterExpr::BinaryOp {
                left: Box::new(left),
                op: BinaryOperator::And,
                right: Box::new(right),
            };
        }
        Ok(left)
    }

    // not_expr = 'not' not_expr | comparison
    fn parse_not(&mut self) -> Result<FilterExpr, ODataError> {
        if self.peek_text_is("not") {
            self.advance();
            let operand = self.parse_not()?;
            return Ok(FilterExpr::UnaryOp {
                op: UnaryOperator::Not,
                operand: Box::new(operand),
            });
        }
        self.parse_comparison()
    }

    // comparison = primary ( comparison_op primary )?
    fn parse_comparison(&mut self) -> Result<FilterExpr, ODataError> {
        let left = self.parse_primary()?;
        if let Some(op) = self.peek_comparison_op() {
            self.advance();
            let right = self.parse_primary()?;
            Ok(FilterExpr::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            })
        } else {
            Ok(left)
        }
    }

    // primary = '(' or_expr ')' | literal | function_call | property
    fn parse_primary(&mut self) -> Result<FilterExpr, ODataError> {
        // Clone the token data to avoid holding an immutable borrow on self
        // while we need to call self.advance().
        let (text, offset) = match self.peek() {
            Some(tok) => (tok.text.clone(), tok.offset),
            None => {
                return Err(ODataError::InvalidFilter {
                    message: "unexpected end of filter expression".into(),
                    position: self.current_offset(),
                });
            }
        };

        // Parenthesized sub-expression
        if text == "(" {
            self.advance();
            let expr = self.parse_or()?;
            self.expect_text(")")?;
            return Ok(expr);
        }

        // String literal
        if text.starts_with('\'') && text.ends_with('\'') && text.len() >= 2 {
            let s = text[1..text.len() - 1].to_string();
            self.advance();
            return Ok(FilterExpr::Literal(ODataValue::String(s)));
        }

        // Numeric literal
        if text.starts_with(|c: char| c.is_ascii_digit() || c == '-') {
            self.advance();
            if text.contains('.') {
                let val: f64 = text.parse().map_err(|_| ODataError::InvalidFilter {
                    message: format!("invalid float literal '{text}'"),
                    position: offset,
                })?;
                return Ok(FilterExpr::Literal(ODataValue::Float(val)));
            } else {
                let val: i64 = text.parse().map_err(|_| ODataError::InvalidFilter {
                    message: format!("invalid integer literal '{text}'"),
                    position: offset,
                })?;
                return Ok(FilterExpr::Literal(ODataValue::Int(val)));
            }
        }

        // Keywords: null, true, false
        if text == "null" {
            self.advance();
            return Ok(FilterExpr::Literal(ODataValue::Null));
        }
        if text == "true" {
            self.advance();
            return Ok(FilterExpr::Literal(ODataValue::Boolean(true)));
        }
        if text == "false" {
            self.advance();
            return Ok(FilterExpr::Literal(ODataValue::Boolean(false)));
        }

        // Identifier: could be a function call or property.
        if text.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            let name = text;
            self.advance();

            // Check for function call: name followed by '('
            if self.peek_text_is("(") {
                self.advance(); // consume '('
                let args = self.parse_argument_list()?;
                self.expect_text(")")?;
                return Ok(FilterExpr::FunctionCall { name, args });
            }

            // Otherwise it's a property reference
            return Ok(FilterExpr::Property(name));
        }

        Err(ODataError::InvalidFilter {
            message: format!("unexpected token '{text}'"),
            position: offset,
        })
    }

    fn parse_argument_list(&mut self) -> Result<Vec<FilterExpr>, ODataError> {
        let mut args = Vec::new();

        // Empty argument list?
        if self.peek_text_is(")") {
            return Ok(args);
        }

        loop {
            args.push(self.parse_or()?);
            if self.peek_text_is(",") {
                self.advance();
            } else {
                break;
            }
        }

        Ok(args)
    }

    // -- Helpers --

    fn peek_text_is(&self, text: &str) -> bool {
        self.peek().map(|t| t.text.as_str() == text).unwrap_or(false)
    }

    fn peek_comparison_op(&self) -> Option<BinaryOperator> {
        self.peek().and_then(|t| match t.text.as_str() {
            "eq" => Some(BinaryOperator::Eq),
            "ne" => Some(BinaryOperator::Ne),
            "gt" => Some(BinaryOperator::Gt),
            "ge" => Some(BinaryOperator::Ge),
            "lt" => Some(BinaryOperator::Lt),
            "le" => Some(BinaryOperator::Le),
            "has" => Some(BinaryOperator::Has),
            _ => None,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- $filter tests -------------------------------------------------------

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
            FilterExpr::BinaryOp { op: BinaryOperator::Or, left, right } => {
                // Left should be the 'and' node
                match left.as_ref() {
                    FilterExpr::BinaryOp { op: BinaryOperator::And, .. } => {}
                    other => panic!("expected And on left, got {other:?}"),
                }
                // Right should be a comparison
                match right.as_ref() {
                    FilterExpr::BinaryOp { op: BinaryOperator::Eq, .. } => {}
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
            FilterExpr::UnaryOp { op: UnaryOperator::Not, operand } => {
                match operand.as_ref() {
                    FilterExpr::BinaryOp { op: BinaryOperator::Eq, .. } => {}
                    other => panic!("expected Eq inside not, got {other:?}"),
                }
            }
            other => panic!("expected Not at top, got {other:?}"),
        }
    }

    #[test]
    fn filter_parenthesized_expression() {
        let expr = parse_filter("(A eq 1 or B eq 2) and C eq 3").unwrap();
        match &expr {
            FilterExpr::BinaryOp { op: BinaryOperator::And, left, .. } => {
                match left.as_ref() {
                    FilterExpr::BinaryOp { op: BinaryOperator::Or, .. } => {}
                    other => panic!("expected Or in parens, got {other:?}"),
                }
            }
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
