//! Reads an action's `effect`: a list of effect statements.

use super::AutomatonParseError;
use crate::predicate::{Effect, parse_effect};
use toml::Value;

const MIGRATE_HINT: &str = "run `temper migrate-predicates` to convert the old effect syntax";

/// Parse `effect = ["items += 1", "ready = true", ...]`.
pub(super) fn parse_effects(
    action: &str,
    value: &Value,
) -> Result<Vec<Effect>, AutomatonParseError> {
    let invalid = |detail: String| {
        let hint = if crate::automaton::legacy::is_legacy_effect(value) {
            format!("; {MIGRATE_HINT}")
        } else {
            String::new()
        };
        AutomatonParseError::Validation(format!("action '{action}' effect: {detail}{hint}"))
    };
    let Value::Array(entries) = value else {
        return Err(invalid(
            "expected a list of effect statements, such as [\"items += 1\"]".into(),
        ));
    };
    entries
        .iter()
        .map(|entry| match entry {
            Value::String(text) => parse_effect(text).map_err(|e| invalid(e.to_string())),
            other => Err(invalid(format!(
                "'{other}' is not an effect statement (expected a string)"
            ))),
        })
        .collect()
}
