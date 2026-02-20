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
pub mod csdl;
pub mod model;

/// TLA+ specification extractor (legacy — prefer [`automaton`] for new specs).
pub mod tlaplus;

// Re-export primary public API at crate root.
pub use automaton::{Automaton, parse_automaton, to_state_machine};
pub use csdl::{CsdlDocument, CsdlParseError, parse_csdl};
pub use model::{SpecModel, SpecSource, build_spec_model, build_spec_model_mixed};
pub use tlaplus::{Invariant, StateMachine, Transition, extract_state_machine};
