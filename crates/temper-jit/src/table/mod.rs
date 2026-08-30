//! Transition tables: state machine transitions as DATA, not code.
//!
//! A [`TransitionTable`] encodes the complete set of transition rules for a single
//! entity type. It can be built from an I/O Automaton TOML spec and evaluated
//! at runtime without any compiled transition logic.

mod builder;
mod evaluate;
#[cfg(test)]
mod failure_routes_test;
pub mod guard;
#[cfg(test)]
mod reference_contract_test;
pub mod types;

pub use guard::{EvalContext, Guard, GuardFailure, GuardFailureKind};
pub use types::{
    ActionInputError, ActionParamMetadata, CompositeActionMetadata, CompositeCedarGate, Effect,
    StateVarMetadata, SubWriteSpec, TransitionResult, TransitionRule, TransitionTable,
};
