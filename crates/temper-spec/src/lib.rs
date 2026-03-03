//! temper-spec: Specification parsers for the Temper framework.
//!
//! Supports two specification formats:
//! - **I/O Automaton TOML** (primary): Lynch-Tuttle precondition/effect style, agent-friendly
//! - **TLA+** (legacy): temporal logic for deep correctness reasoning
//! - **CSDL** (data model): OData v4 Common Schema Definition Language
//!
//! Both I/O Automaton and TLA+ compile to the same [`StateMachine`] intermediate
//! representation, which feeds the verification cascade and runtime.

pub mod automaton;
pub mod cross_invariant;
pub mod csdl;
pub mod model;

/// TLA+ specification extractor (legacy — prefer [`automaton`] for new specs).
pub mod tlaplus;

// Re-export primary public API at crate root.
pub use automaton::{
    Automaton, LintFinding, LintSeverity, lint_automaton, parse_automaton, parse_bool_initial,
    parse_counter_initial_usize, parse_list_initial, parse_var_initial_json, to_state_machine,
};
pub use cross_invariant::{
    CrossInvariant, CrossInvariantLintFinding, CrossInvariantLintSeverity,
    CrossInvariantParseError, CrossInvariantSpec, DeletePolicy, InvariantKind, RelationOverride,
    lint_cross_invariants, parse_cross_invariants, parse_related_status_in_assert,
};
pub use csdl::{CsdlDocument, CsdlParseError, parse_csdl};
pub use model::{SpecModel, SpecSource, build_spec_model, build_spec_model_mixed};
pub use tlaplus::{Invariant, StateMachine, Transition, extract_state_machine};
