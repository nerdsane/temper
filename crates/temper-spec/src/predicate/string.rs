//! Concrete strings and fresh equality symbols.

use std::fmt;

use super::Sym;

/// A concrete spec string or a fresh equality symbol. Symbols never equal
/// literals, including literals whose spelling is `#1`.
#[derive(
    Clone, Copy, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum SymbolicString {
    /// A string from a spec or a concrete input.
    Literal(Sym),
    /// An arbitrary nonblank value absent from the spec's literals.
    Symbol(usize),
    /// An arbitrary nonempty whitespace-only value absent from the spec's literals.
    BlankSymbol(usize),
}

/// Where a made-up value's class sits in its id, past any number of values
/// a walk could hold at once.
const CLASS_SHIFT: u32 = 16;

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
    /// A made-up value: the `number`th of its class. Two classes never hold
    /// the same value, so a state need not say how values of classes nothing
    /// compares relate. Class 0 is every value of a model with no classes.
    pub fn in_class(class: usize, number: usize, blank: bool) -> Self {
        let id = class << CLASS_SHIFT | number;
        if blank {
            Self::BlankSymbol(id)
        } else {
            Self::Symbol(id)
        }
    }
    /// The class of a made-up value.
    pub fn class(&self) -> Option<usize> {
        self.symbol_id().map(|id| id >> CLASS_SHIFT)
    }
    /// A made-up value's number within its class.
    pub fn number(&self) -> Option<usize> {
        self.symbol_id().map(|id| id & ((1 << CLASS_SHIFT) - 1))
    }
    /// Its value for predicate evaluation.
    pub fn val(&self) -> super::eval::Val<'_> {
        match self {
            Self::Literal(s) => super::eval::Val::Str(s.as_str()),
            Self::Symbol(n) | Self::BlankSymbol(n) => super::eval::Val::Symbol(*n),
        }
    }
}
impl From<String> for SymbolicString {
    fn from(s: String) -> Self {
        Self::Literal(s.into())
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
            Self::Symbol(n) | Self::BlankSymbol(n) => {
                f.write_str(if matches!(self, Self::Symbol(_)) {
                    "#"
                } else {
                    "#blank"
                })?;
                match n >> CLASS_SHIFT {
                    0 => write!(f, "{n}"),
                    class => write!(f, "{class}.{}", n & ((1 << CLASS_SHIFT) - 1)),
                }
            }
        }
    }
}
