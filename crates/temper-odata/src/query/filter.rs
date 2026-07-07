//! `$filter` recursive-descent parser.
//!
//! Tokenizes and parses OData `$filter` expressions into a [`FilterExpr`] AST.

use super::types::{BinaryOperator, FilterExpr, ODataValue, UnaryOperator};
use crate::error::ODataError;

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
            let mut closed = false;
            while i < chars.len() {
                if chars[i] == '\'' {
                    // Check for escaped quote ''
                    if i + 1 < chars.len() && chars[i + 1] == '\'' {
                        s.push('\'');
                        i += 2;
                    } else {
                        i += 1;
                        closed = true;
                        break;
                    }
                } else {
                    s.push(chars[i]);
                    i += 1;
                }
            }
            // An unterminated literal (no closing quote) must be rejected rather
            // than silently accepted as if it were closed.
            if !closed {
                return Err(ODataError::InvalidFilter {
                    message: "unterminated string literal".into(),
                    position: offset,
                });
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
            || (chars[i] == '-' && i + 1 < chars.len() && chars[i + 1].is_ascii_digit())
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

        // Identifiers and keywords (dotted names like 'guid', property paths).
        //
        // '$' is deliberately NOT an identifier-start character. It is not valid
        // in a property path, and accepting it here while the loop below does not
        // consume it previously spun forever without advancing `i` — a
        // denial-of-service on any `$filter` value containing a bare '$'. A stray
        // '$' now falls through to the "unexpected character" error below (400).
        if chars[i].is_ascii_alphabetic() || chars[i] == '_' {
            let mut word = String::new();
            while i < chars.len()
                && (chars[i].is_ascii_alphanumeric()
                    || chars[i] == '_'
                    || chars[i] == '.'
                    || chars[i] == '/'
                    || chars[i] == '-')
            {
                word.push(chars[i]);
                i += 1;
            }
            // Invariant: the identifier-start set (alphabetic | '_') is a subset
            // of the continue set above, so the loop always consumes at least one
            // character. This guarantees the tokenizer makes forward progress.
            debug_assert!(i > offset, "tokenizer failed to advance on identifier");
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

/// Maximum nesting depth for a `$filter` expression.
///
/// The recursive-descent parser descends one level per parenthesized
/// sub-expression, `not` operand, and function-call argument. Without a bound, a
/// crafted filter such as `((((…))))` or `not not not …` would recurse until the
/// request thread's stack overflows and the process aborts — a denial-of-service
/// reachable from the public OData query surface. A well-formed query never
/// nests anywhere near this deep.
const MAX_FILTER_DEPTH: usize = 128;

struct FilterParser<'a> {
    tokens: &'a [Token],
    pos: usize,
    /// Current recursion depth, bounded by [`MAX_FILTER_DEPTH`].
    depth: usize,
}

impl<'a> FilterParser<'a> {
    fn new(tokens: &'a [Token]) -> Self {
        Self {
            tokens,
            pos: 0,
            depth: 0,
        }
    }

    /// Enter one nesting level, rejecting filters that nest past
    /// [`MAX_FILTER_DEPTH`] before the recursion can overflow the stack.
    ///
    /// Every recursive descent (parentheses, `not`, function arguments) must be
    /// wrapped in a matching [`descend`](Self::descend)/[`ascend`](Self::ascend)
    /// pair so that width (many siblings) does not count as depth.
    fn descend(&mut self) -> Result<(), ODataError> {
        self.depth += 1;
        if self.depth > MAX_FILTER_DEPTH {
            return Err(ODataError::InvalidFilter {
                message: format!(
                    "filter expression nesting exceeds maximum depth of {MAX_FILTER_DEPTH}"
                ),
                position: self.current_offset(),
            });
        }
        Ok(())
    }

    /// Leave a nesting level previously entered via [`descend`](Self::descend).
    fn ascend(&mut self) {
        self.depth = self.depth.saturating_sub(1);
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
            self.descend()?;
            let operand = self.parse_not();
            self.ascend();
            let operand = operand?;
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
            self.descend()?;
            let expr = self.parse_or();
            self.ascend();
            let expr = expr?;
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
            self.descend()?;
            let arg = self.parse_or();
            self.ascend();
            args.push(arg?);
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
        self.peek()
            .map(|t| t.text.as_str() == text)
            .unwrap_or(false)
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

    // -- Security regression tests (ARN-176) ---------------------------------
    //
    // Two denial-of-service bugs on the public OData `$filter` surface. Each
    // test documents the pre-fix behavior; all now return an `InvalidFilter`
    // error (surfaced as HTTP 400) quickly instead of hanging or crashing the
    // request thread.

    #[test]
    fn filter_dollar_sign_returns_error_not_hang() {
        // Pre-fix: the tokenizer accepted '$' as an identifier-start character,
        // but the identifier loop never consumed it, so `i` never advanced — an
        // infinite loop that also grew the token vec without bound, hanging (and
        // eventually OOM-ing) the request thread. The watchdog below turns that
        // hang into a test failure instead of blocking the suite forever. On a
        // regression the spawned thread is intentionally left detached (it never
        // returns); the harness process exits and reaps it after the failure.
        use std::sync::mpsc;
        use std::time::Duration;

        let (tx, rx) = mpsc::channel();
        // A stray '$' outside a string literal is the trigger. Inside quotes it
        // is fine (handled by the string-literal branch).
        std::thread::spawn(move || {
            let _ = tx.send(parse_filter("Name eq $foo").is_err());
        });

        match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(is_err) => assert!(
                is_err,
                "'$' in a filter value must return InvalidFilter, not succeed"
            ),
            Err(_) => {
                panic!("parse_filter hung on '$' — tokenizer infinite loop is not fixed")
            }
        }
    }

    #[test]
    fn filter_bare_dollar_sign_is_rejected() {
        // A lone '$' is an unexpected character, not an identifier.
        assert!(parse_filter("$").is_err());
    }

    #[test]
    fn filter_deeply_nested_parens_returns_error_not_overflow() {
        // Pre-fix: unbounded recursion (parse_primary -> parse_or per '(')
        // overflowed the request thread's stack and aborted the process. Now
        // bounded: returns InvalidFilter once nesting passes MAX_FILTER_DEPTH.
        let depth = 100_000;
        let mut input = String::with_capacity(depth * 2 + 16);
        for _ in 0..depth {
            input.push('(');
        }
        input.push_str("Name eq 'x'");
        for _ in 0..depth {
            input.push(')');
        }
        assert!(
            parse_filter(&input).is_err(),
            "deeply nested parens must return InvalidFilter, not overflow the stack"
        );
    }

    #[test]
    fn filter_deeply_nested_not_returns_error_not_overflow() {
        // Pre-fix: `not not not …` recursed parse_not without bound and
        // overflowed the stack. Now bounded past MAX_FILTER_DEPTH.
        let mut input = String::new();
        for _ in 0..100_000 {
            input.push_str("not ");
        }
        input.push_str("Active eq true");
        assert!(
            parse_filter(&input).is_err(),
            "deeply nested 'not' must return InvalidFilter, not overflow the stack"
        );
    }

    #[test]
    fn filter_deeply_nested_functions_returns_error_not_overflow() {
        // Pre-fix: `f(f(f(…)))` recursed parse_argument_list -> parse_or without
        // bound and overflowed the stack. Now bounded past MAX_FILTER_DEPTH.
        let depth = 100_000;
        let mut input = String::with_capacity(depth * 2 + 8);
        for _ in 0..depth {
            input.push_str("f(");
        }
        input.push('1');
        for _ in 0..depth {
            input.push(')');
        }
        assert!(
            parse_filter(&input).is_err(),
            "deeply nested function calls must return InvalidFilter, not overflow"
        );
    }

    #[test]
    fn filter_moderately_nested_parens_still_parse() {
        // Nesting well within MAX_FILTER_DEPTH must still parse — the depth
        // bound must not reject legitimate queries.
        let depth = MAX_FILTER_DEPTH / 2;
        let mut input = String::new();
        for _ in 0..depth {
            input.push('(');
        }
        input.push_str("Name eq 'x'");
        for _ in 0..depth {
            input.push(')');
        }
        assert!(
            parse_filter(&input).is_ok(),
            "nesting within the depth bound must still parse"
        );

        // A wide, shallow filter (many OR-ed comparisons) must not trip the
        // depth bound — width is not depth.
        let wide = (0..500)
            .map(|n| format!("Id eq {n}"))
            .collect::<Vec<_>>()
            .join(" or ");
        assert!(
            parse_filter(&wide).is_ok(),
            "wide filters must not be rejected"
        );
    }

    #[test]
    fn filter_unterminated_string_returns_error() {
        // Pre-fix: an unterminated literal was silently accepted as if closed.
        assert!(parse_filter("Name eq 'foo").is_err());
    }

    #[test]
    fn filter_depth_bound_is_inclusive_at_the_limit() {
        // Exercise the exact boundary so an off-by-one (e.g. flipping `>` to
        // `>=`) is caught: MAX_FILTER_DEPTH levels of nesting must parse, and one
        // level deeper must be rejected.
        let nest = |levels: usize| {
            let mut s = String::with_capacity(levels * 2 + 16);
            for _ in 0..levels {
                s.push('(');
            }
            s.push_str("Name eq 'x'");
            for _ in 0..levels {
                s.push(')');
            }
            s
        };
        assert!(
            parse_filter(&nest(MAX_FILTER_DEPTH)).is_ok(),
            "exactly MAX_FILTER_DEPTH levels must be accepted"
        );
        assert!(
            parse_filter(&nest(MAX_FILTER_DEPTH + 1)).is_err(),
            "one level past MAX_FILTER_DEPTH must be rejected"
        );
    }
}
