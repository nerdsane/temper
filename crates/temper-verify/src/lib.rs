//! temper-verify: Model checking, simulation, and property-based testing.
//!
//! Four-level verification cascade:
//! 0. **SMT symbolic verification** — Z3-based algebraic verification
//! 1. **Stateright model checking** — exhaustive state-space exploration
//! 2. **Deterministic simulation** — FoundationDB/TigerBeetle-style fault injection
//! 3. **Property-based tests** — random action sequences with invariant checking

pub mod cascade;
pub mod checker;
pub mod model;
pub mod proptest_gen;
pub mod simulation;
pub mod smt;

// Re-export key types.
pub use cascade::{ActorSimResult, CascadeLevel, CascadeResult, LevelResult, VerificationCascade};
pub use checker::{check_model, VerificationResult};
pub use model::{
    build_model_from_ioa, InvariantKind, ModelEffect, ModelGuard, ResolvedTransition,
    TemperModel, TemperModelAction, TemperModelState,
};
pub use proptest_gen::{run_prop_tests_from_ioa, PropTestResult};
pub use simulation::{
    run_multi_seed_simulation_from_ioa, run_simulation_from_ioa, LivenessViolation, SimConfig,
    SimulationResult,
};
pub use smt::{verify_symbolic, SmtResult};
