//! temper-spec: Specification parsers for the Temper framework.
//!
//! - **I/O Automaton TOML** (behavior): Lynch-Tuttle precondition/effect style.
//!   Parsed once to [`Automaton`]. Downstream crates derive verification and
//!   runtime views from that value (`TemperModel`, `TransitionTable`).
//! - **CSDL** (data model): OData v4 Common Schema Definition Language.
//!
//! TLA+ is not an authored format. The extractor and `StateMachine` IR were
//! removed (ADR-0169).

pub mod automaton;
pub mod cross_invariant;
pub mod csdl;
pub mod model;
pub mod naming;

// Re-export primary public API at crate root.
pub use automaton::{
    Automaton, FieldInvariant, FieldPredicate, LintFinding, LintSeverity, lint_automaton,
    parse_automaton, parse_bool_initial, parse_counter_initial_usize, parse_list_initial,
    parse_var_initial_json,
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
