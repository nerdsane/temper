//! temper-verify: Model checking and property-based testing for Temper.
//!
//! Uses Stateright for exhaustive model checking and proptest for
//! property-based testing of TLA+ behavioral specifications.
//!
//! # Architecture
//!
//! The verification pipeline:
//! 1. Parse a TLA+ spec into a `StateMachine` (via `temper-spec`)
//! 2. Build a `TemperModel` from the state machine (`model` module)
//! 3. Run exhaustive model checking (`checker` module)
//! 4. Orchestrate multi-level verification (`cascade` module)

pub mod model;
pub mod checker;
pub mod cascade;

// Re-export key types for convenience.
pub use model::{TemperModel, TemperModelState, TemperModelAction, build_model, build_model_from_tla};
pub use checker::{VerificationResult, check_model};
pub use cascade::{VerificationCascade, CascadeResult, CascadeLevel, LevelResult};
