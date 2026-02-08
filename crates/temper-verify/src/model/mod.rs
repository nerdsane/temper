//! Generate Stateright models from I/O Automaton specifications.
//!
//! This module translates a `temper_spec::tlaplus::StateMachine` (the unified IR
//! for both IOA and TLA+ sources) into a Stateright `Model` that can be
//! exhaustively explored by a model checker. The generated model captures:
//!   - Status-based states with an item counter
//!   - Transitions as named actions with source/target state guards
//!   - Safety invariants as Stateright "always" properties
//!
//! Because Stateright's `Property::always` requires a bare function pointer
//! (not a capturing closure), all invariant data lives inside `TemperModel`
//! and is accessed via the `&TemperModel` reference in property conditions.

pub mod builder;
mod stateright_impl;
pub mod types;

pub use builder::{build_model, build_model_from_ioa, build_model_from_tla, build_model_with_max_items};
pub use types::{
    InvariantKind, ResolvedInvariant, ResolvedTransition, TemperModel, TemperModelAction,
    TemperModelState,
};
