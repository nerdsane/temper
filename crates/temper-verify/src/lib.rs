//! How verified: L0–L3 cascade. Must not become a second kernel.
//!
//! `ModelEffect` / `apply_effects` in `model/semantics.rs` is a third interpreter.
//! Production apply is `temper-server` `entity_actor/effects.rs`.
//!
//! Four-level verification cascade:
//! 0. **SMT symbolic verification** — Z3-based algebraic verification
//! 1. **Stateright model checking** — exhaustive state-space exploration
//! 2. **Deterministic simulation** — FoundationDB/TigerBeetle-style fault injection
//! 3. **Property-based tests** — random action sequences with invariant checking

pub mod budget;
pub mod can_reach;
pub mod cascade;
pub mod checker;
pub mod composite;
pub mod diagnostics;
pub mod lost_places;
pub mod model;
pub mod paths;
pub mod proptest_gen;
mod reach;
pub mod reaches;
pub mod simulation;
pub mod smt;

// Re-export key types.
pub use budget::{Budget, Clock};
pub use cascade::{
    ActorSimResult, CascadeLevel, CascadeResult, KeptKey, LevelResult, VerificationCascade,
};
pub use checker::{Reached, VerificationResult, check_model};
pub use composite::{CompositePlanError, CompositeVerificationPlan};
pub use lost_places::LostPlace;
pub use model::{
    ModelEffect, ParamValue, ResolvedTransition, TemperModel, TemperModelAction, TemperModelState,
    build_model_from_ioa,
};
pub use paths::{
    PathExtractionConfig, PathExtractionResult, PathStep, ReachablePath, extract_paths,
};
pub use proptest_gen::{PropTestResult, run_prop_tests_from_ioa};
pub use simulation::{
    LivenessViolation, SimConfig, SimulationResult, run_multi_seed_simulation,
    run_multi_seed_simulation_from_ioa, run_simulation_from_ioa,
};
pub use smt::{SmtResult, verify_symbolic};
