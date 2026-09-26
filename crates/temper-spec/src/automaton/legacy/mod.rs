//! The predicate syntax that preceded [`crate::predicate`]: guard strings and
//! tables, `assert` + `when`, trigger-guard tables and field-predicate
//! tables. Kept only to convert old specs to the current grammar.

mod lower;

pub use lower::{
    LoweredInvariant, field_invariant_to_expr, field_predicate_to_expr, guard_to_expr,
    guards_to_expr, invariant_to_expr, status_in, trigger_guard_to_expr,
};

#[cfg(test)]
#[path = "lower_test.rs"]
mod tests;
