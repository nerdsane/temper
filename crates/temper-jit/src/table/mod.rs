//! Transition tables: state machine transitions as DATA, not code.
//!
//! A [`TransitionTable`] encodes the complete set of transition rules for a single
//! entity type. It can be built from an I/O Automaton TOML spec and evaluated
//! at runtime without any compiled transition logic.

mod builder;
mod evaluate;
pub mod types;

pub use types::{
    CompositeActionMetadata, CompositeCedarGate, Effect, EvalContext, Guard, StateVarMetadata,
    SubWriteSpec, TransitionResult, TransitionRule, TransitionTable,
};
