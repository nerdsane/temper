//! JIT transition tables and hot-swap execution for Temper entity state machines.

pub mod params;
pub mod shadow;
pub mod swap;
pub mod table;

// Re-export primary types at crate root.
pub use params::{ParamContractError, restrict_to_declared_params, undeclared_param_keys};
pub use shadow::{Mismatch, ShadowResult, TestCase, shadow_test};
pub use swap::{SwapController, SwapResult};
pub use table::{
    Effect, EvalContext, Guard, GuardFailure, GuardFailureKind, TransitionResult, TransitionRule,
    TransitionTable,
};
