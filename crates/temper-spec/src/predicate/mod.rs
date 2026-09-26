//! The predicate language shared by every condition in an IOA spec: action
//! guards, trigger guards, `[[invariant]]` and `[[field_invariant]]` asserts.
//!
//! One grammar ([`parse`]), one tree ([`Expr`]), one evaluator ([`eval`]).
//! Expressions serialize as their canonical source text.

mod ast;
mod check;
mod eval;
mod parse;
mod print;

pub use ast::{CmpOp, Expr, Literal, Name, Operand, Set};
pub use check::{Scope, VarKind, check, unmodelable};
pub use eval::{Env, Truth, Val, eval, explain};
pub use parse::{MAX_DEPTH, ParseError, is_keyword, parse};

impl std::str::FromStr for Expr {
    type Err = ParseError;

    fn from_str(source: &str) -> Result<Self, Self::Err> {
        parse(source)
    }
}

impl serde::Serialize for Expr {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for Expr {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let source = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        parse(&source).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests;
