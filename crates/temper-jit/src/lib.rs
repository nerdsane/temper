//! JIT transition tables and hot-swap execution for Temper entity state machines.

pub mod shadow;
pub mod swap;
pub mod table;

// Re-export primary types at crate root.
pub use shadow::{Mismatch, ShadowResult, TestCase, shadow_test};
pub use swap::{SwapController, SwapResult};
pub use table::{Effect, EvalContext, Guard, TransitionResult, TransitionRule, TransitionTable};
