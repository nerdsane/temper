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

mod assignments;
pub mod builder;
mod count_bounds;
mod counters;
mod entry;
mod expansion;
pub(crate) mod heap;
mod parameters;
pub mod semantics;
mod silent;
mod stateright_impl;
mod string;
mod strings;
pub mod types;

pub use builder::{build_model_from_automaton, build_model_from_ioa, build_model_tracking};
pub(crate) use entry::{broken_by_action, entry_broken};
pub(crate) use stateright_impl::{ENTRY_RULES, REACHES_TERMINAL};
pub(crate) use types::ExploreLimit;
pub use types::{
    LivenessKind, ModelEffect, ModelString, ParamValue, ResolvedEntryRule, ResolvedInvariant,
    ResolvedLiveness, ResolvedTransition, StringParam, TemperModel, TemperModelAction,
    TemperModelState,
};
