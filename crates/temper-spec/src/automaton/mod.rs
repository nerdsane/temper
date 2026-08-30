//! I/O Automaton specification format for Temper entities.
//!
//! Based on Lynch-Tuttle I/O Automata (1987): a labeled state transition system
//! with input, output, and internal actions, each specified by precondition/effect pairs.
//!
//! This replaces TLA+ as the primary agent-facing specification format because:
//! - **Precondition/effect** maps 1:1 to TransitionTable guards and effects
//! - **Input/output/internal** action classification maps to the actor model
//! - **TOML format** is natively readable/writable by LLM agents
//! - **Composition** via shared actions maps to actor message passing
//! - **No temporal logic overhead** — we use Stateright for model checking
//!
//! TLA+ remains available for humans who want temporal reasoning.

pub mod assert_parser;
mod collection_contract;
mod failure_routes;
pub mod field_invariant;
mod initial;
mod lint;
pub mod metadata;
pub mod parser;
mod reference_contract;
mod reference_contract_lint;
mod toml_parser;
pub mod translate;
pub mod trigger_graph;
mod types;

pub use assert_parser::{AssertCompareOp, ParsedAssert, parse_assert_expr};
pub use failure_routes::{ResolvedFailureRoute, resolve_failure_routes};
pub use field_invariant::{FieldInvariant, FieldPredicate, PredicateParseError};
pub use initial::{
    parse_bool_initial, parse_counter_initial_usize, parse_list_initial, parse_var_initial_json,
};
pub use lint::{
    BundleLintFinding, LintFinding, LintSeverity, lint_automata_bundle, lint_automata_csdl_bundle,
    lint_automaton,
};
pub use metadata::{LivenessViolation, SpecMetadata};
pub use parser::{
    LivenessEnforcement, LivenessViolationReporter, parse_automaton, parse_automaton_with_liveness,
    set_liveness_violation_reporter, to_state_machine,
};
pub use reference_contract::MAX_REFERENCE_TARGETS_PER_WRITE;
pub use reference_contract_lint::lint_csdl_reference_contracts;
pub use translate::{ResolvedAction, ResolvedEffect, ResolvedGuard, translate_actions};
pub use trigger_graph::{TriggerEdge, TriggerGraph};
pub use types::*;
