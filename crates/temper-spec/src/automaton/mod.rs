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

mod types;
mod parser;

pub use types::*;
pub use parser::parse_automaton;
