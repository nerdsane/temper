//! TLA+ specification extractor (legacy).
//!
//! This module parses TLA+ source into a [`StateMachine`] intermediate
//! representation. The types (`StateMachine`, `Transition`, `Invariant`) are
//! shared with the IOA path and remain in active use as the common IR.
//!
//! For new specifications, prefer I/O Automaton TOML (see [`super::automaton`]).
//! The TLA+ *extractor* is retained for existing specs and deep temporal reasoning.

mod extractor;
mod types;

pub use extractor::{TlaExtractError, extract_state_machine};
pub use types::*;
