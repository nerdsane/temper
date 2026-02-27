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

mod initial;
mod lint;
pub mod parser;
mod toml_parser;
mod types;

pub use initial::{
    parse_bool_initial, parse_counter_initial_usize, parse_list_initial, parse_var_initial_json,
};
pub use lint::{LintFinding, LintSeverity, lint_automaton};
pub use parser::{parse_automaton, to_state_machine};
pub use types::*;
