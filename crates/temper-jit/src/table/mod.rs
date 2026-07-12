//! Transition tables: state machine transitions as DATA, not code.
//!
//! A [`TransitionTable`] encodes the complete set of transition rules for a single
//! entity type. It can be built from an I/O Automaton TOML spec and evaluated
//! at runtime without any compiled transition logic.

mod builder;
mod effects;
mod evaluate;
pub mod guard;
pub mod types;

pub use effects::{
    EffectExecution, EffectState, ScheduleAtRequest, ScheduledAction, SpawnRequest, apply_effects,
    build_eval_context as build_effect_eval_context,
};
pub use guard::{EvalContext, Guard, GuardFailure, GuardFailureKind};
pub use types::{
    CompositeActionMetadata, CompositeCedarGate, Effect, StateVarMetadata, SubWriteSpec,
    TransitionResult, TransitionRule, TransitionTable,
};
