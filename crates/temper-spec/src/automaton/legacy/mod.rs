//! The predicate syntax that preceded [`crate::predicate`]: guard strings and
//! tables, `assert` + `when`, trigger-guard tables and field-predicate
//! tables. Kept only to convert old specs to the current grammar
//! ([`migrate_source`]).

mod assert_parser;
mod field_predicate;
mod guard_syntax;
mod lower;
mod migrate;
mod syntax;

pub use lower::{LoweredInvariant, invariant_to_expr};
pub use migrate::{Migration, migrate_source, simplify};

/// Whether `value` is an action guard in the old syntax (a clause such as
/// `"is_true x"`, a `{ type = ... }` table, or an array of either).
pub(crate) fn is_legacy_guard(value: &toml::Value) -> bool {
    let mut guards = Vec::new();
    guard_syntax::parse_guard_value(value, &mut guards).is_ok()
}

#[cfg(test)]
#[path = "lower_test.rs"]
mod tests;
