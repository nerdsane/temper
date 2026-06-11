//! I/O Automaton specification format for Temper entities.
//!
//! Based on Lynch-Tuttle I/O Automata (1987): a labeled state transition system
//! with input, output, and internal actions, each specified by precondition/effect pairs.
//!
//! This replaced TLA+ as the agent-facing specification format because:
//! - **Precondition/effect** maps 1:1 to TransitionTable guards and effects
//! - **Input/output/internal** action classification maps to the actor model
//! - **TOML format** is natively readable/writable by LLM agents
//! - **Composition** via shared actions maps to actor message passing
//! - **No temporal logic overhead** — we use Stateright for model checking

pub mod assert_parser;
pub mod field_invariant;
mod initial;
mod lint;
pub mod metadata;
pub mod parser;
mod toml_parser;
pub mod translate;
pub mod trigger_graph;
mod types;

pub use assert_parser::{AssertCompareOp, ParsedAssert, parse_assert_expr};
pub use field_invariant::{FieldInvariant, FieldPredicate, PredicateParseError};
pub use initial::{
    parse_bool_initial, parse_counter_initial_usize, parse_list_initial, parse_var_initial_json,
};
pub use lint::{
    BundleLintFinding, LintFinding, LintSeverity, lint_automata_bundle, lint_automaton,
};
pub use metadata::{LivenessViolation, SpecMetadata};
pub use parser::{
    LivenessEnforcement, LivenessViolationReporter, parse_automaton, parse_automaton_with_liveness,
    set_liveness_violation_reporter, to_state_machine,
};
pub use translate::{ResolvedAction, ResolvedEffect, ResolvedGuard, translate_actions};
pub use trigger_graph::{TriggerEdge, TriggerGraph};
pub use types::*;
