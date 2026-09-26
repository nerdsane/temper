//! Transition tables: state machine transitions as DATA, not code.
//!
//! A [`TransitionTable`] encodes the complete set of transition rules for a single
//! entity type. It can be built from an I/O Automaton TOML spec and evaluated
//! at runtime without any compiled transition logic.

pub mod action_contract;
mod builder;
pub mod effect_args;
mod evaluate;
pub mod guard;
pub mod types;

pub use guard::{EvalContext, GuardFailure, Related, RelatedMap};
pub use temper_spec::predicate::Expr;
pub use types::{
    CompositeActionMetadata, CompositeCedarGate, Effect, StateVarMetadata, SubWriteSpec,
    TransitionResult, TransitionRule, TransitionTable,
};
