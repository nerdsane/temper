//! Lexer and recursive-descent parser for predicate expressions.
//!
//! ```text
//! expr    = implies ;
//! implies = or [ "=>" implies ] ;
//! or      = and { "||" and } ;
//! and     = unary { "&&" unary } ;
//! unary   = "!" unary | atom ;
//! atom    = "(" expr ")" | "empty" "(" ident ")" | term ;
//! term    = operand [ cmp operand | [ "not" ] "in" set ] ;
//! operand = "status" | ident | ident "[" ident "]" "." "status"
//!         | "len" "(" ident ")" | literal ;
//! set     = "[" [ literal { "," literal } ] "]" | ident ;
//! literal = int | 'string' | "true" | "false" | "null" ;
//! ```

use super::ast::{CmpOp, Expr, Literal, Operand, Set};

/// Maximum nesting depth of parentheses and `!` (TigerStyle budget: the
/// parser recurses once per level).
pub const MAX_DEPTH: usize = 64;

/// A parse failure with the byte offset where it was detected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid predicate '{source_text}': {message} at column {}", .offset + 1)]
pub struct ParseError {
    /// The full expression being parsed.
    pub source_text: String,
    /// Byte offset of the failure.
    pub offset: usize,
    /// What was expected or found.
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Int(i64),
    Str(String),
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Dot,
    Not,
    AndAnd,
    OrOr,
    Implies,
    Cmp(CmpOp),
}

/// Parse a predicate expression.
pub fn parse(source: &str) -> Result<Expr, ParseError> {
    let tokens = lex(source)?;
    let mut parser = Parser {
        source,
        tokens,
        pos: 0,
        depth: 0,
    };
    let expr = parser.expr()?;
    if parser.pos < parser.tokens.len() {
        return Err(parser.error("unexpected token after expression"));
    }
    Ok(expr)
}

fn lex(source: &str) -> Result<Vec<(Tok, usize)>, ParseError> {
    let err = |offset: usize, message: &str| ParseError {
        source_text: source.to_string(),
        offset,
        message: message.to_string(),
    };
    let bytes = source.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let start = i;
        let two = bytes.get(i..i + 2);
        let (tok, width) = match bytes[i] {
            b' ' | b'\t' | b'\n' | b'\r' => {
                i += 1;
                continue;
            }
            b'(' => (Tok::LParen, 1),
            b')' => (Tok::RParen, 1),
            b'[' => (Tok::LBracket, 1),
            b']' => (Tok::RBracket, 1),
            b',' => (Tok::Comma, 1),
            b'.' => (Tok::Dot, 1),
            _ if two == Some(b"&&") => (Tok::AndAnd, 2),
            _ if two == Some(b"||") => (Tok::OrOr, 2),
            _ if two == Some(b"=>") => (Tok::Implies, 2),
            _ if two == Some(b"==") => (Tok::Cmp(CmpOp::Eq), 2),
            _ if two == Some(b"!=") => (Tok::Cmp(CmpOp::Ne), 2),
            _ if two == Some(b"<=") => (Tok::Cmp(CmpOp::Le), 2),
            _ if two == Some(b">=") => (Tok::Cmp(CmpOp::Ge), 2),
            b'<' => (Tok::Cmp(CmpOp::Lt), 1),
            b'>' => (Tok::Cmp(CmpOp::Gt), 1),
            b'!' => (Tok::Not, 1),
            b'\'' => {
                let body = start + 1;
                let end = source[body..]
                    .find('\'')
                    .map(|offset| body + offset)
                    .ok_or_else(|| err(start, "unterminated string"))?;
                (Tok::Str(source[body..end].to_string()), end + 1 - start)
            }
            b'-' | b'0'..=b'9' => {
                let end = scan(bytes, start + 1, |b| b.is_ascii_digit());
                let value = source[start..end]
                    .parse::<i64>()
                    .map_err(|_| err(start, "invalid integer"))?;
                (Tok::Int(value), end - start)
            }
            b if b.is_ascii_alphabetic() || b == b'_' => {
                let end = scan(bytes, start + 1, |b| b.is_ascii_alphanumeric() || b == b'_');
                (Tok::Ident(source[start..end].to_string()), end - start)
            }
            b'"' => return Err(err(start, "strings use single quotes")),
            _ => return Err(err(start, "unexpected character")),
        };
        i += width;
        tokens.push((tok, start));
    }
    Ok(tokens)
}

/// Index of the first byte at or after `from` that fails `keep`.
fn scan(bytes: &[u8], from: usize, keep: impl Fn(u8) -> bool) -> usize {
    let mut end = from;
    while end < bytes.len() && keep(bytes[end]) {
        end += 1;
    }
    end
}

struct Parser<'a> {
    source: &'a str,
    tokens: Vec<(Tok, usize)>,
    pos: usize,
    depth: usize,
}

impl Parser<'_> {
    fn error(&self, message: &str) -> ParseError {
        let offset = self
            .tokens
            .get(self.pos)
            .map(|(_, offset)| *offset)
            .unwrap_or(self.source.len());
        ParseError {
            source_text: self.source.to_string(),
            offset,
            message: message.to_string(),
        }
    }

    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos).map(|(tok, _)| tok)
    }

    fn peek_keyword(&self, keyword: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(name)) if name == keyword)
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.peek() == Some(tok) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, tok: &Tok, what: &str) -> Result<(), ParseError> {
        if self.eat(tok) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {what}")))
        }
    }

    fn enter(&mut self) -> Result<(), ParseError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error("expression nested too deeply"));
        }
        Ok(())
    }

    fn expr(&mut self) -> Result<Expr, ParseError> {
        self.enter()?;
        let lhs = self.or()?;
        let result = if self.eat(&Tok::Implies) {
            Expr::Implies(Box::new(lhs), Box::new(self.expr()?))
        } else {
            lhs
        };
        self.depth -= 1;
        Ok(result)
    }

    fn or(&mut self) -> Result<Expr, ParseError> {
        let mut parts = vec![self.and()?];
        while self.eat(&Tok::OrOr) {
            parts.push(self.and()?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Expr::Or(parts)
        })
    }

    fn and(&mut self) -> Result<Expr, ParseError> {
        let mut parts = vec![self.unary()?];
        while self.eat(&Tok::AndAnd) {
            parts.push(self.unary()?);
        }
        Ok(if parts.len() == 1 {
            parts.remove(0)
        } else {
            Expr::And(parts)
        })
    }

    fn unary(&mut self) -> Result<Expr, ParseError> {
        if self.eat(&Tok::Not) {
            self.enter()?;
            let inner = self.unary()?;
            self.depth -= 1;
            return Ok(Expr::Not(Box::new(inner)));
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<Expr, ParseError> {
        if self.eat(&Tok::LParen) {
            let inner = self.expr()?;
            self.expect(&Tok::RParen, "')'")?;
            return Ok(inner);
        }
        if self.peek_keyword("empty") {
            self.pos += 1;
            let name = self.call_arg()?;
            return Ok(Expr::Empty(name));
        }
        self.term()
    }

    fn term(&mut self) -> Result<Expr, ParseError> {
        let lhs = self.operand()?;
        if let Some(Tok::Cmp(op)) = self.peek().cloned() {
            self.pos += 1;
            let rhs = self.operand()?;
            return Ok(Expr::Compare { lhs, op, rhs });
        }
        let negated = self.peek_keyword("not");
        if negated {
            self.pos += 1;
            if !self.peek_keyword("in") {
                return Err(self.error("expected 'in' after 'not'"));
            }
        }
        if self.peek_keyword("in") {
            self.pos += 1;
            let set = self.set()?;
            return Ok(Expr::In {
                value: lhs,
                set,
                negated,
            });
        }
        match lhs {
            Operand::Var(name) => Ok(Expr::Var(name)),
            Operand::Lit(Literal::Bool(value)) => Ok(Expr::Const(value)),
            _ => Err(self.error("expected a comparison or 'in'")),
        }
    }

    fn call_arg(&mut self) -> Result<String, ParseError> {
        self.expect(&Tok::LParen, "'('")?;
        let name = self.ident()?;
        self.expect(&Tok::RParen, "')'")?;
        Ok(name)
    }

    fn ident(&mut self) -> Result<String, ParseError> {
        match self.peek().cloned() {
            Some(Tok::Ident(name)) if !is_keyword(&name) => {
                self.pos += 1;
                Ok(name)
            }
            _ => Err(self.error("expected a name")),
        }
    }

    fn operand(&mut self) -> Result<Operand, ParseError> {
        let Some(tok) = self.peek().cloned() else {
            return Err(self.error("unexpected end of expression"));
        };
        match tok {
            Tok::Int(value) => {
                self.pos += 1;
                Ok(Operand::Lit(Literal::Int(value)))
            }
            Tok::Str(value) => {
                self.pos += 1;
                Ok(Operand::Lit(Literal::Str(value)))
            }
            Tok::Ident(name) => {
                self.pos += 1;
                match name.as_str() {
                    "status" => Ok(Operand::Status),
                    "true" => Ok(Operand::Lit(Literal::Bool(true))),
                    "false" => Ok(Operand::Lit(Literal::Bool(false))),
                    "null" => Ok(Operand::Lit(Literal::Null)),
                    "len" => Ok(Operand::Len(self.call_arg()?)),
                    _ if is_keyword(&name) => {
                        self.pos -= 1;
                        Err(self.error(&format!("unexpected keyword '{name}'")))
                    }
                    _ if self.eat(&Tok::LBracket) => {
                        let id_field = self.ident()?;
                        self.expect(&Tok::RBracket, "']'")?;
                        self.expect(&Tok::Dot, "'.status'")?;
                        if !self.peek_keyword("status") {
                            return Err(
                                self.error("only '.status' can be read from a related entity")
                            );
                        }
                        self.pos += 1;
                        Ok(Operand::CrossStatus {
                            entity_type: name,
                            id_field,
                        })
                    }
                    _ => Ok(Operand::Var(name)),
                }
            }
            _ => Err(self.error("expected a name or value")),
        }
    }

    fn set(&mut self) -> Result<Set, ParseError> {
        if !self.eat(&Tok::LBracket) {
            return Ok(Set::Var(self.ident()?));
        }
        let mut items = Vec::new();
        if self.eat(&Tok::RBracket) {
            return Ok(Set::List(items));
        }
        loop {
            match self.operand()? {
                Operand::Lit(lit) => items.push(lit),
                _ => return Err(self.error("list entries must be literals")),
            }
            if self.eat(&Tok::RBracket) {
                return Ok(Set::List(items));
            }
            self.expect(&Tok::Comma, "',' or ']'")?;
        }
    }
}

/// Reserved words that cannot be used as names.
pub fn is_keyword(name: &str) -> bool {
    matches!(
        name,
        "status" | "true" | "false" | "null" | "in" | "not" | "len" | "empty"
    )
}
