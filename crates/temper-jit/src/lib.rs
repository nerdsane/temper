//! temper-jit: JIT compilation and hot-swappable state machine execution for Temper.
//!
//! Instead of compiled Rust code, transitions are represented as data (transition tables)
//! interpreted at runtime. This enables Tier 2 optimization: change how entities behave
//! without redeployment.
//!
//! # Modules
//!
//! - [`table`] — Transition tables and rules: state machine transitions as DATA.
//! - [`swap`] — Hot-swap protocol for live-updating transition tables.
//! - [`shadow`] — Shadow testing: compare old and new tables for observational equivalence.

pub mod shadow;
pub mod swap;
pub mod table;

// Re-export primary types at crate root.
pub use shadow::{Mismatch, ShadowResult, TestCase, shadow_test};
pub use swap::{SwapController, SwapResult};
pub use table::{Effect, EvalContext, Guard, TransitionResult, TransitionRule, TransitionTable};
