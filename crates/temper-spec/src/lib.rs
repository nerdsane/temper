//! temper-spec: Specification parsers for the Temper framework.
//!
//! Supports two specification formats:
//! - **I/O Automaton TOML** (primary, agent-facing): Lynch-Tuttle precondition/effect style
//! - **TLA+** (secondary, human-facing): temporal logic for deep correctness reasoning
//! - **CSDL** (data model): OData v4 Common Schema Definition Language
//!
//! Both I/O Automaton and TLA+ compile to the same `StateMachine` intermediate
//! representation, which feeds the verification cascade and runtime.

pub mod csdl;
pub mod tlaplus;
pub mod automaton;
pub mod model;

// Re-export primary public API at crate root.
pub use csdl::{parse_csdl, CsdlDocument, CsdlParseError};
pub use tlaplus::{extract_state_machine, StateMachine, Transition, Invariant};
pub use automaton::{parse_automaton, Automaton};
pub use model::{build_spec_model, SpecModel};
