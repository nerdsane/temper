//! Budgets for cross-entity reaction dispatch.
//!
//! Reactions are declared as entity-kind `[[action.triggers]]` on the
//! source action; the server synthesizes and dispatches them.

/// Maximum cascade depth for recursive reaction dispatch.
pub const MAX_REACTION_DEPTH: u32 = 8;
