//! Transition tables: state machine transitions as DATA, not code.
//!
//! A [`TransitionTable`] encodes the complete set of transition rules for a single
//! entity type. It can be built from an I/O Automaton TOML spec and evaluated
//! at runtime without any compiled transition logic.

mod builder;
mod evaluate;
pub mod guard;
#[cfg(test)]
mod reference_contract_test;
pub mod types;

pub use guard::{EvalContext, Guard, GuardFailure, GuardFailureKind};
pub use types::{
    CompositeActionMetadata, CompositeCedarGate, Effect, StateVarMetadata, SubWriteSpec,
    TransitionResult, TransitionRule, TransitionTable,
};
