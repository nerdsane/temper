//! temper-verify: Model checking, simulation, and property-based testing.
//!
//! Three-level verification cascade:
//! 1. **Stateright model checking** — exhaustive state-space exploration
//! 2. **Deterministic simulation** — FoundationDB/TigerBeetle-style fault injection
//! 3. **Property-based tests** — random action sequences with invariant checking

pub mod model;
pub mod checker;
pub mod cascade;
pub mod simulation;
pub mod proptest_gen;

// Re-export key types.
pub use model::{TemperModel, TemperModelState, TemperModelAction, ResolvedTransition, build_model, build_model_from_ioa, build_model_from_tla};
pub use checker::{VerificationResult, check_model};
pub use cascade::{VerificationCascade, CascadeResult, CascadeLevel, LevelResult, ActorSimResult};
pub use simulation::{SimConfig, SimulationResult, run_simulation, run_simulation_from_ioa, run_multi_seed_simulation, run_multi_seed_simulation_from_ioa};
pub use proptest_gen::{PropTestResult, run_prop_tests, run_prop_tests_from_ioa};
