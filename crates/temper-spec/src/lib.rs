//! temper-spec: Specification parsers for the Temper framework.
//!
//! Supports two specification formats:
//! - **I/O Automaton TOML** (behavior): Lynch-Tuttle precondition/effect style, agent-friendly
//! - **CSDL** (data model): OData v4 Common Schema Definition Language
//!
//! I/O Automaton specs compile to the [`StateMachine`] intermediate
//! representation, which feeds the verification cascade and runtime.

pub mod automaton;
pub mod cross_invariant;
pub mod csdl;
pub mod loader;
pub mod model;
pub mod naming;
pub mod state_machine;

// Re-export primary public API at crate root.
pub use automaton::{
    Automaton, FieldInvariant, FieldPredicate, LintFinding, LintSeverity, lint_automaton,
    parse_automaton, parse_bool_initial, parse_counter_initial_usize, parse_list_initial,
    parse_var_initial_json, to_state_machine,
};
pub use cross_invariant::{
    CrossInvariant, CrossInvariantLintFinding, CrossInvariantLintSeverity, CrossInvariantOperator,
    CrossInvariantParseError, CrossInvariantSpec, DeletePolicy, InvariantKind, RelatedFieldAssert,
    RelationOverride, lint_cross_invariants, parse_cross_invariants, parse_related_field_assert,
    parse_related_status_in_assert,
};
pub use csdl::{CsdlDocument, CsdlParseError, parse_csdl};
pub use model::{SpecModel, build_spec_model};
pub use naming::{to_pascal_case, to_snake_case};
pub use state_machine::{Invariant, LivenessProperty, StateMachine, Transition};
