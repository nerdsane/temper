//! Canonical TOML parser for I/O Automaton specifications.
//!
//! The complete document is deserialized exactly once into [`Automaton`].
//! Compatibility helpers accept the intentionally supported legacy string
//! syntax for guards and effects without weakening the structured schema.

mod compatibility;

use super::parser::AutomatonParseError;
use super::types::Automaton;

pub(super) use compatibility::{
    deserialize_boolish, deserialize_cedar_gate, deserialize_copy_fields, deserialize_effects,
    deserialize_guards,
};

/// Parse one complete TOML document into the canonical automaton schema.
pub(super) fn parse_toml_to_automaton(input: &str) -> Result<Automaton, AutomatonParseError> {
    toml::from_str(input).map_err(|error| AutomatonParseError::Toml(error.to_string()))
}
