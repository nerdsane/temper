//! What is defined: `TransitionTable`, `Effect`, guards, hot-swap, shadow test.
//!
//! Apply is not here. Production apply is `temper-server` `entity_actor/effects.rs`.
//! Postgres actors and verify each have their own interpreter.

pub mod shadow;
pub mod swap;
pub mod table;

// Re-export primary types at crate root.
pub use shadow::{Mismatch, ShadowResult, TestCase, shadow_test};
pub use swap::{SwapController, SwapResult};
pub use table::{
    Effect, EvalContext, Guard, GuardFailure, GuardFailureKind, TransitionResult, TransitionRule,
    TransitionTable,
};
