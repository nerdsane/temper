//! Concrete strings and fresh equality symbols.

use std::fmt;

/// A concrete spec string or a fresh equality symbol. Symbols never equal
/// literals, including literals whose spelling is `#1`.
#[derive(
    Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum SymbolicString {
    /// A string from a spec or a concrete input.
    Literal(String),
    /// An arbitrary nonblank value absent from the spec's literals.
    Symbol(usize),
    /// An arbitrary nonempty whitespace-only value absent from the spec's literals.
    BlankSymbol(usize),
}

impl SymbolicString {
    /// Whether this is the empty concrete string.
    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Literal(s) if s.is_empty())
    }
    /// Whether this is an empty or whitespace-only string, refused
    /// by `param_nonempty`, including symbolic whitespace inputs.
    pub fn is_blank(&self) -> bool {
        match self {
            Self::Literal(s) => s.trim().is_empty(),
            Self::Symbol(_) => false,
            Self::BlankSymbol(_) => true,
        }
    }
    /// Its equality identity, shared by blank and nonblank symbols.
    pub fn symbol_id(&self) -> Option<usize> {
        match self {
            Self::Literal(_) => None,
            Self::Symbol(n) | Self::BlankSymbol(n) => Some(*n),
        }
    }
    /// Its value for predicate evaluation.
    pub fn val(&self) -> super::eval::Val<'_> {
        match self {
            Self::Literal(s) => super::eval::Val::Str(s),
            Self::Symbol(n) | Self::BlankSymbol(n) => super::eval::Val::Symbol(*n),
        }
    }
}
impl From<String> for SymbolicString {
    fn from(s: String) -> Self {
        Self::Literal(s)
    }
}
impl From<&str> for SymbolicString {
    fn from(s: &str) -> Self {
        Self::Literal(s.into())
    }
}
impl fmt::Display for SymbolicString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(s) => s.fmt(f),
            Self::Symbol(n) => write!(f, "#{n}"),
            Self::BlankSymbol(n) => write!(f, "#blank{n}"),
        }
    }
}
