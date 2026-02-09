//! Generate Stateright models from I/O Automaton specifications.
//!
//! This module translates a parsed `Automaton` directly into a Stateright `Model`
//! for exhaustive state-space exploration. The generated model captures:
//!   - Multi-variable state (status + named counters + named booleans)
//!   - Transitions as named actions with guards and effects
//!   - Safety invariants as Stateright "always" properties
//!   - Liveness properties as "eventually" / "always" properties
//!
//! Because Stateright's `Property::always` requires a bare function pointer
//! (not a capturing closure), all invariant data lives inside `TemperModel`
//! and is accessed via the `&TemperModel` reference in property conditions.

pub mod builder;
mod stateright_impl;
pub mod types;

pub use builder::{build_model_from_automaton, build_model_from_ioa};
pub use types::{
    InvariantKind, LivenessKind, ModelEffect, ModelGuard, ResolvedInvariant, ResolvedLiveness,
    ResolvedTransition, TemperModel, TemperModelAction, TemperModelState,
};
