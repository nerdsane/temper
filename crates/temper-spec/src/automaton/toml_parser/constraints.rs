//! Parse action contracts without accepting unknown constraint forms.
use super::{AutomatonParseError, isolate_action_sections};
use crate::automaton::ActionConstraint;
use std::collections::BTreeMap;

pub(super) fn extract(
    source: &str,
) -> Result<BTreeMap<String, Vec<ActionConstraint>>, AutomatonParseError> {
    #[derive(serde::Deserialize)]
    struct Wrapper {
        #[serde(default, rename = "action")]
        actions: Vec<Entry>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
        #[serde(default)]
        constraints: Vec<ActionConstraint>,
    }
    let isolated = isolate_action_sections(source);
    if isolated.trim().is_empty() {
        return Ok(BTreeMap::new());
    }
    let wrapper: Wrapper = toml::from_str(&isolated)
        .map_err(|error| AutomatonParseError::Toml(format!("action.constraints: {error}")))?;
    let mut entries = BTreeMap::new();
    for action in wrapper.actions {
        entries
            .entry(action.name)
            .or_insert_with(Vec::new)
            .extend(action.constraints);
    }
    Ok(entries)
}
