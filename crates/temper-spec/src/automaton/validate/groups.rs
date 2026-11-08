//! Checks on the statechart labels: each `says` holds words, and the
//! `[[group]]` boxes form a tree over the declared states.

use std::collections::{BTreeMap, BTreeSet};

use super::super::parser::AutomatonParseError;
use super::super::types::Automaton;

/// Check every action's and group's `says`.
pub(super) fn validate_says(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    for action in &automaton.actions {
        check_says(&format!("action '{}'", action.name), action.says.as_deref())?;
    }
    for group in &automaton.groups {
        check_says(&format!("group '{}'", group.name), group.says.as_deref())?;
    }
    Ok(())
}

/// A label that holds no words draws as a blank arrow or box; leave `says`
/// out instead and the reader shows the name.
fn check_says(scope: &str, says: Option<&str>) -> Result<(), AutomatonParseError> {
    match says {
        Some(says) if says.trim().is_empty() => Err(AutomatonParseError::Validation(format!(
            "{scope}: says must not be empty"
        ))),
        _ => Ok(()),
    }
}

/// Check that groups are uniquely named apart from states, hold at least one
/// entry, list only declared states and groups, give every entry at most one
/// group, and never contain themselves.
pub(super) fn validate_groups(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    let invalid = |message: String| Err(AutomatonParseError::Validation(message));
    let states: BTreeSet<&str> = automaton
        .automaton
        .states
        .iter()
        .map(String::as_str)
        .collect();
    let mut groups: BTreeSet<&str> = BTreeSet::new();
    for group in &automaton.groups {
        if states.contains(group.name.as_str()) {
            return invalid(format!(
                "group '{}' has the name of a state; a group name must differ from every state",
                group.name
            ));
        }
        if !groups.insert(group.name.as_str()) {
            return invalid(format!("group '{}' is declared twice", group.name));
        }
        if group.states.is_empty() {
            return invalid(format!(
                "group '{}' lists no states; a group boxes at least one state or group",
                group.name
            ));
        }
    }

    // Each state or group has at most one parent, so the parents form a forest
    // unless they loop.
    let mut parent: BTreeMap<&str, &str> = BTreeMap::new();
    for group in &automaton.groups {
        for entry in &group.states {
            let entry = entry.as_str();
            if !states.contains(entry) && !groups.contains(entry) {
                return invalid(format!(
                    "group '{}' lists '{entry}', which is not a declared state or group",
                    group.name
                ));
            }
            if let Some(first) = parent.insert(entry, group.name.as_str()) {
                let place = if first == group.name {
                    format!("twice in group '{first}'")
                } else {
                    format!("in group '{first}' and in group '{}'", group.name)
                };
                return invalid(format!(
                    "'{entry}' is listed {place}; a state or group belongs to at most one group"
                ));
            }
        }
    }

    for group in &automaton.groups {
        if let Some(cycle) = cycle_through(group.name.as_str(), &parent) {
            let path = cycle
                .iter()
                .map(|name| format!("'{name}'"))
                .collect::<Vec<_>>()
                .join(" holds ");
            return invalid(format!(
                "groups form a cycle: {path}; a group cannot contain itself"
            ));
        }
    }
    Ok(())
}

/// The groups from `start` around to `start` again, outermost first, when
/// following parents from `start` comes back to it.
fn cycle_through<'a>(start: &'a str, parent: &BTreeMap<&'a str, &'a str>) -> Option<Vec<&'a str>> {
    let mut chain = vec![start];
    let mut at = start;
    // Every step reaches a different group unless the chain loops, so the
    // walk ends within one step per entry.
    for _ in 0..=parent.len() {
        at = parent.get(at)?;
        chain.push(at);
        if at == start {
            chain.reverse();
            return Some(chain);
        }
    }
    None
}
