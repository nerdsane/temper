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
pub mod paths;
pub mod proptest_gen;
pub mod simulation;
pub mod smt;

// Re-export key types.
pub use cascade::{ActorSimResult, CascadeLevel, CascadeResult, LevelResult, VerificationCascade};
pub use checker::{VerificationResult, check_model};
pub use model::{
    InvariantKind, ModelEffect, ModelGuard, ResolvedTransition, TemperModel, TemperModelAction,
    TemperModelState, build_model_from_ioa,
};
pub use paths::{
    PathExtractionConfig, PathExtractionResult, PathStep, ReachablePath, extract_paths,
};
pub use proptest_gen::{PropTestResult, run_prop_tests_from_ioa};
pub use simulation::{
    LivenessViolation, SimConfig, SimulationResult, run_multi_seed_simulation_from_ioa,
    run_simulation_from_ioa,
};
pub use smt::{SmtResult, verify_symbolic};
