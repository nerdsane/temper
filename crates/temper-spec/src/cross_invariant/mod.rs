//! Cross-entity invariant specification support.
//!
//! This module parses and lints `cross-invariants.toml`, a tenant-scoped
//! specification file for relation policies and cross-entity invariants.

mod lint;
mod parser;
mod types;

pub use lint::{CrossInvariantLintFinding, CrossInvariantLintSeverity, lint_cross_invariants};
pub use parser::{
    CrossInvariantParseError, RelatedStatusInAssert, parse_cross_invariants,
    parse_related_status_in_assert,
};
pub use types::{
    CrossInvariant, CrossInvariantSpec, DeletePolicy, InvariantKind, RelationOverride,
};
