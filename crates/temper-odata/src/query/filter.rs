//! `$filter` recursive-descent parser.
//!
//! Tokenizes and parses OData `$filter` expressions into a [`FilterExpr`] AST.

use super::types::{BinaryOperator, FilterExpr, ODataValue, UnaryOperator};
use crate::error::ODataError;

mod budget;

use budget::FilterBudget;
#[cfg(test)]
use budget::{
    FILTER_ARGUMENT_BUDGET, FILTER_INPUT_BYTE_BUDGET, FILTER_LITERAL_BYTE_BUDGET,
    FILTER_NODE_BUDGET, FILTER_OPERATOR_BUDGET, FILTER_TOKEN_BUDGET,
};

/// Parse a `$filter` expression string into a [`FilterExpr`] AST.
pub fn parse_filter(input: &str) -> Result<FilterExpr, ODataError> {
    let mut budget = FilterBudget::new(input)?;
    let tokens = tokenize_filter(input, &mut budget)?;
    let mut parser = FilterParser::new(&tokens, budget);
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

fn tokenize_filter(input: &str, budget: &mut FilterBudget) -> Result<Vec<Token>, ODataError> {
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
            budget.consume_literal_bytes(s.len(), offset)?;
            push_token(&mut tokens, budget, format!("'{s}'"), offset)?;
            continue;
        }

        // Parentheses and comma
        if chars[i] == '(' || chars[i] == ')' || chars[i] == ',' {
            push_token(&mut tokens, budget, chars[i].to_string(), offset)?;
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
            push_token(&mut tokens, budget, num, offset)?;
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
            push_token(&mut tokens, budget, word, offset)?;
            continue;
        }

        return Err(ODataError::InvalidFilter {
            message: format!("unexpected character '{}'", chars[i]),
            position: i,
        });
    }

    Ok(tokens)
}

fn push_token(
    tokens: &mut Vec<Token>,
    budget: &mut FilterBudget,
    text: String,
    offset: usize,
) -> Result<(), ODataError> {
    budget.consume_token(offset)?;
    tokens.push(Token { text, offset });
    Ok(())
}

// -- Recursive descent parser ------------------------------------------------

/// Nesting-depth budget for a `$filter` expression.
///
/// The recursive-descent parser descends one level per parenthesized
/// sub-expression, `not` operand, and function-call argument. Without a bound, a
/// crafted filter such as `((((…))))` or `not not not …` would recurse until the
/// request thread's stack overflows and the process aborts — a denial-of-service
/// reachable from the public OData query surface. A well-formed query never
/// nests anywhere near this deep.
// Keep ample headroom below the request worker's stack: filter canaries
// exercise the accepted boundary on 512 KiB, rather than merely fitting a
// default 2 MiB worker before the surrounding request frames are considered.
const FILTER_DEPTH_BUDGET: usize = 32;

struct FilterParser<'a> {
    tokens: &'a [Token],
    pos: usize,
    /// Current recursion depth, bounded by [`FILTER_DEPTH_BUDGET`].
    depth: usize,
    budget: FilterBudget,
}

impl<'a> FilterParser<'a> {
    fn new(tokens: &'a [Token], budget: FilterBudget) -> Self {
        Self {
            tokens,
            pos: 0,
            depth: 0,
            budget,
        }
    }

    /// Enter one nesting level, rejecting filters that nest past
    /// [`FILTER_DEPTH_BUDGET`] before the recursion can overflow the stack.
    ///
    /// Every recursive descent (parentheses, `not`, function arguments) must be
    /// wrapped in a matching [`descend`](Self::descend)/[`ascend`](Self::ascend)
    /// pair so that width (many siblings) does not count as depth.
    fn descend(&mut self) -> Result<(), ODataError> {
        if self.depth == FILTER_DEPTH_BUDGET {
            return Err(ODataError::InvalidFilter {
                message: format!(
                    "filter expression nesting exceeds depth budget of {FILTER_DEPTH_BUDGET}"
                ),
                position: self.current_offset(),
            });
        }
        self.depth += 1;
        Ok(())
    }

    /// Leave a nesting level previously entered via [`descend`](Self::descend).
    fn ascend(&mut self) {
        assert!(self.depth > 0, "filter parser depth underflow");
        self.depth -= 1;
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
        let mut operands = vec![self.parse_and()?];
        while self.peek_text_is("or") {
            let operator_offset = self.current_offset();
            self.budget.consume_operator(operator_offset)?;
            self.budget.consume_node(operator_offset)?;
            self.advance();
            operands.push(self.parse_and()?);
        }
        Ok(balance_associative(operands, BinaryOperator::Or))
    }

    // and_expr = not_expr ( 'and' not_expr )*
    fn parse_and(&mut self) -> Result<FilterExpr, ODataError> {
        let mut operands = vec![self.parse_not()?];
        while self.peek_text_is("and") {
            let operator_offset = self.current_offset();
            self.budget.consume_operator(operator_offset)?;
            self.budget.consume_node(operator_offset)?;
            self.advance();
            operands.push(self.parse_not()?);
        }
        Ok(balance_associative(operands, BinaryOperator::And))
    }

    // not_expr = 'not' not_expr | comparison
    fn parse_not(&mut self) -> Result<FilterExpr, ODataError> {
        if self.peek_text_is("not") {
            let operator_offset = self.current_offset();
            self.budget.consume_operator(operator_offset)?;
            self.budget.consume_node(operator_offset)?;
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
            let operator_offset = self.current_offset();
            self.budget.consume_operator(operator_offset)?;
            self.budget.consume_node(operator_offset)?;
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
            self.budget.consume_node(offset)?;
            self.advance();
            let s = text[1..text.len() - 1].to_string();
            return Ok(FilterExpr::Literal(ODataValue::String(s)));
        }

        // Numeric literal
        if text.starts_with(|c: char| c.is_ascii_digit() || c == '-') {
            self.budget.consume_node(offset)?;
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
            self.budget.consume_node(offset)?;
            self.advance();
            return Ok(FilterExpr::Literal(ODataValue::Null));
        }
        if text == "true" {
            self.budget.consume_node(offset)?;
            self.advance();
            return Ok(FilterExpr::Literal(ODataValue::Boolean(true)));
        }
        if text == "false" {
            self.budget.consume_node(offset)?;
            self.advance();
            return Ok(FilterExpr::Literal(ODataValue::Boolean(false)));
        }

        // Identifier: could be a function call or property.
        if text.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            let name = text;
            self.advance();

            // Check for function call: name followed by '('
            if self.peek_text_is("(") {
                self.budget.consume_node(offset)?;
                self.advance(); // consume '('
                let args = self.parse_argument_list()?;
                self.expect_text(")")?;
                return Ok(FilterExpr::FunctionCall { name, args });
            }

            // Otherwise it's a property reference
            self.budget.consume_node(offset)?;
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
            self.budget.consume_argument(self.current_offset())?;
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

/// Build an order-preserving balanced tree for an associative boolean operator.
///
/// A left fold makes an accepted wide filter's AST as deep as its width. Every
/// downstream consumer then inherits that attacker-controlled recursion depth,
/// including in-memory evaluation, SQL translation, and `Drop`. Pairing adjacent
/// operands keeps the same left-to-right order and boolean meaning while bounding
/// tree depth logarithmically.
fn balance_associative(mut operands: Vec<FilterExpr>, op: BinaryOperator) -> FilterExpr {
    assert!(
        !operands.is_empty(),
        "boolean chain must contain an operand"
    );

    while operands.len() > 1 {
        let mut next_level = Vec::with_capacity(operands.len().div_ceil(2));
        let mut iter = operands.into_iter();
        while let Some(left) = iter.next() {
            if let Some(right) = iter.next() {
                next_level.push(FilterExpr::BinaryOp {
                    left: Box::new(left),
                    op,
                    right: Box::new(right),
                });
            } else {
                next_level.push(left);
            }
        }
        operands = next_level;
    }

    debug_assert_eq!(operands.len(), 1, "balancing must produce one root");
    operands.remove(0)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[path = "filter/basic_tests.rs"]
mod basic_tests;

#[cfg(test)]
#[path = "filter/security_tests.rs"]
mod security_tests;
