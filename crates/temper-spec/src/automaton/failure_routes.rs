//! Typed trigger-failure route validation and resolution.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use super::parser::AutomatonParseError;
use super::{Action, ActionTrigger, Automaton, FailureCategory, FailureRoute};

/// One validated category-to-callback binding carried into production metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedFailureRoute {
    /// Source action owning the fallible trigger.
    pub source_action: String,
    /// Trigger name within the source action.
    pub trigger_name: String,
    /// Closed v1 category selecting the route.
    pub category: FailureCategory,
    /// Ordinary source-entity callback action.
    pub callback_action: String,
}

/// Resolve every typed route in deterministic source declaration order.
pub fn resolve_failure_routes(
    automaton: &Automaton,
) -> Result<Vec<ResolvedFailureRoute>, AutomatonParseError> {
    let mut resolved = Vec::new();
    for source_action in &automaton.actions {
        for trigger in &source_action.triggers {
            validate_trigger(automaton, source_action, trigger)?;
            for route in &trigger.failure_routes {
                let callback = resolve_callback(automaton, source_action, trigger, route)?;
                resolved.push(ResolvedFailureRoute {
                    source_action: source_action.name.clone(),
                    trigger_name: trigger.name.clone(),
                    category: route.category,
                    callback_action: callback.name.clone(),
                });
            }
        }
    }
    Ok(resolved)
}

pub(super) fn validate_trigger(
    automaton: &Automaton,
    source_action: &Action,
    trigger: &ActionTrigger,
) -> Result<(), AutomatonParseError> {
    if trigger.failure_routes.is_empty() {
        return Ok(());
    }
    if trigger.on_failure.is_some() {
        return Err(validation(format!(
            "trigger '{}' on action '{}' cannot mix typed failure_routes with legacy on_failure",
            trigger.name, source_action.name
        )));
    }

    let mut categories = BTreeSet::new();
    for route in &trigger.failure_routes {
        if !categories.insert(route.category) {
            return Err(validation(format!(
                "trigger '{}' on action '{}' declares failure category '{:?}' more than once",
                trigger.name, source_action.name, route.category
            )));
        }
        let callback = resolve_callback(automaton, source_action, trigger, route)?;
        validate_callback_signature(source_action, trigger, callback)?;
    }
    Ok(())
}

fn resolve_callback<'a>(
    automaton: &'a Automaton,
    source_action: &Action,
    trigger: &ActionTrigger,
    route: &FailureRoute,
) -> Result<&'a Action, AutomatonParseError> {
    match (&route.action, &route.to_state) {
        (Some(_), Some(_)) | (None, None) => Err(validation(format!(
            "failure route '{:?}' on trigger '{}' action '{}' must declare exactly one of action or to_state",
            route.category, trigger.name, source_action.name
        ))),
        (Some(callback_name), None) => resolve_named_callback(
            automaton,
            source_action,
            trigger,
            route.category,
            callback_name,
        ),
        (None, Some(target_state)) => resolve_state_callback(
            automaton,
            source_action,
            trigger,
            route.category,
            target_state,
        ),
    }
}

fn resolve_named_callback<'a>(
    automaton: &'a Automaton,
    source_action: &Action,
    trigger: &ActionTrigger,
    category: FailureCategory,
    callback_name: &str,
) -> Result<&'a Action, AutomatonParseError> {
    if callback_name == source_action.name {
        return Err(validation(format!(
            "failure route '{category:?}' on trigger '{}' cannot replay source action '{}'",
            trigger.name, source_action.name
        )));
    }
    let callback = automaton
        .actions
        .iter()
        .find(|candidate| candidate.name == callback_name)
        .ok_or_else(|| {
            validation(format!(
                "failure route '{category:?}' on trigger '{}' action '{}' references unknown callback action '{}'",
                trigger.name, source_action.name, callback_name
            ))
        })?;
    validate_callback_enabled_after_source(source_action, trigger, callback)?;
    Ok(callback)
}

fn resolve_state_callback<'a>(
    automaton: &'a Automaton,
    source_action: &Action,
    trigger: &ActionTrigger,
    category: FailureCategory,
    target_state: &str,
) -> Result<&'a Action, AutomatonParseError> {
    if !automaton
        .automaton
        .states
        .iter()
        .any(|state| state == target_state)
    {
        return Err(validation(format!(
            "failure route '{category:?}' on trigger '{}' action '{}' references undeclared to_state '{}'",
            trigger.name, source_action.name, target_state
        )));
    }
    let committed_state = source_action.to.as_deref().ok_or_else(|| {
        validation(format!(
            "failure route '{category:?}' on trigger '{}' cannot resolve to_state because source action '{}' has no committed target state",
            trigger.name, source_action.name
        ))
    })?;
    let candidates = automaton
        .actions
        .iter()
        .filter(|candidate| {
            candidate.name != source_action.name
                && candidate.to.as_deref() == Some(target_state)
                && candidate.from.iter().any(|state| state == committed_state)
        })
        .collect::<Vec<_>>();
    if candidates.len() != 1 {
        let names = candidates
            .iter()
            .map(|candidate| candidate.name.as_str())
            .collect::<Vec<_>>();
        return Err(validation(format!(
            "failure route '{category:?}' on trigger '{}' action '{}' to_state '{}' resolves to {} callback actions {:?}; expected exactly one",
            trigger.name,
            source_action.name,
            target_state,
            candidates.len(),
            names
        )));
    }
    Ok(candidates[0])
}

fn validate_callback_enabled_after_source(
    source_action: &Action,
    trigger: &ActionTrigger,
    callback: &Action,
) -> Result<(), AutomatonParseError> {
    let committed_state = source_action.to.as_deref().ok_or_else(|| {
        validation(format!(
            "failure route on trigger '{}' cannot target callback '{}' because source action '{}' has no committed target state",
            trigger.name, callback.name, source_action.name
        ))
    })?;
    if !callback.from.iter().any(|state| state == committed_state) {
        return Err(validation(format!(
            "failure route on trigger '{}' targets callback '{}' which is not enabled from source action '{}' committed state '{}'",
            trigger.name, callback.name, source_action.name, committed_state
        )));
    }
    Ok(())
}

fn validate_callback_signature(
    source_action: &Action,
    trigger: &ActionTrigger,
    callback: &Action,
) -> Result<(), AutomatonParseError> {
    let valid = callback.params.len() == 1
        && callback.params[0].name() == "failure"
        && callback.params[0].param_type() == "failure_v1";
    if !valid {
        return Err(validation(format!(
            "failure route on trigger '{}' action '{}' resolves to callback '{}' which must declare exactly params = [{{ name = \"failure\", type = \"failure_v1\" }}]",
            trigger.name, source_action.name, callback.name
        )));
    }
    Ok(())
}

fn validation(message: String) -> AutomatonParseError {
    AutomatonParseError::Validation(message)
}
