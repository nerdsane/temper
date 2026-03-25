//! Parse I/O Automaton TOML specifications.
//!
//! Also provides conversion to the existing TemperModel and TransitionTable
//! formats, so the verification cascade and runtime work unchanged.
//!
//! The hand-rolled TOML parser lives in [`super::toml_parser`] to keep this
//! module focused on the public API and validation logic.

use super::toml_parser;
use super::types::*;
use crate::tlaplus::{Invariant as TlaInvariant, StateMachine, Transition};

/// Errors from parsing an automaton specification.
#[derive(Debug, thiserror::Error)]
pub enum AutomatonParseError {
    #[error("TOML parse error: {0}")]
    Toml(String),
    #[error("validation error: {0}")]
    Validation(String),
}

/// Parse an I/O Automaton specification from TOML.
pub fn parse_automaton(toml_str: &str) -> Result<Automaton, AutomatonParseError> {
    // TOML parsing — we use a minimal manual approach since we don't have
    // the toml crate. Parse the TOML as serde_json via a two-step conversion.
    // For now, use serde_json with our own simple TOML-to-JSON converter.
    let automaton: Automaton = toml_parser::parse_toml_to_automaton(toml_str)?;
    validate(&automaton)?;
    Ok(automaton)
}

/// Convert an I/O Automaton to the legacy StateMachine format.
///
/// This allows the existing verification cascade (Stateright, DST, proptest)
/// and the TransitionTable builder to work unchanged.
pub fn to_state_machine(automaton: &Automaton) -> StateMachine {
    let transitions = automaton
        .actions
        .iter()
        .filter(|a| a.kind != "output") // Output actions don't transition state
        .map(|a| {
            let from_states = if a.from.is_empty() {
                // Input actions are always enabled (I/O automata property)
                if a.kind == "input" {
                    automaton.automaton.states.clone()
                } else {
                    vec![]
                }
            } else {
                a.from.clone()
            };

            Transition {
                name: a.name.clone(),
                from_states,
                to_state: a.to.clone(),
                guard_expr: format_guards(&a.guard),
                has_parameters: !a.params.is_empty(),
                effect_expr: format_effects(&a.effect),
            }
        })
        .collect();

    let invariants = automaton
        .invariants
        .iter()
        .map(|inv| {
            // Encode `when` states as a trigger prefix so the model builder
            // can extract trigger_states via extract_trigger_states().
            let trigger = if inv.when.is_empty() {
                String::new()
            } else {
                let states = inv
                    .when
                    .iter()
                    .map(|s| format!("\"{s}\""))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("status \\in {{{states}}} => ")
            };
            TlaInvariant {
                name: inv.name.clone(),
                expr: format!("{trigger}{}", inv.assert),
            }
        })
        .collect();

    StateMachine {
        module_name: automaton.automaton.name.clone(),
        states: automaton.automaton.states.clone(),
        transitions,
        invariants,
        liveness_properties: vec![],
        constants: vec![],
        variables: automaton.state.iter().map(|s| s.name.clone()).collect(),
    }
}

fn format_guards(guards: &[Guard]) -> String {
    guards
        .iter()
        .map(|g| match g {
            Guard::StateIn { values } => {
                format!(
                    "status \\in {{{}}}",
                    values
                        .iter()
                        .map(|s| format!("\"{s}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Guard::MinCount { var, min } => format!("{var} >= {min}"),
            Guard::MaxCount { var, max } => format!("{var} < {max}"),
            Guard::IsTrue { var } => format!("{var} = TRUE"),
            Guard::IsFalse { var } => format!("{var} = FALSE"),
            Guard::ListContains { var, value } => format!("{value} \\in {var}"),
            Guard::ListLengthMin { var, min } => format!("Len({var}) >= {min}"),
            Guard::CrossEntityState {
                entity_type,
                entity_id_source,
                required_status,
            } => {
                format!(
                    "{entity_type}[{entity_id_source}].status \\in {{{}}}",
                    required_status
                        .iter()
                        .map(|s| format!("\"{s}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
        })
        .collect::<Vec<_>>()
        .join(" /\\ ")
}

fn format_effects(effects: &[Effect]) -> String {
    effects
        .iter()
        .map(|e| match e {
            Effect::Increment { var } => format!("{var}' = {var} + 1"),
            Effect::Decrement { var } => format!("{var}' = {var} - 1"),
            Effect::SetBool { var, value } => {
                format!("{var}' = {}", if *value { "TRUE" } else { "FALSE" })
            }
            Effect::Emit { event } => format!("Emit(\"{event}\")"),
            Effect::Trigger { name } => format!("Trigger(\"{name}\")"),
            Effect::Schedule {
                action,
                delay_seconds,
            } => format!("Schedule(\"{action}\", {delay_seconds})"),
            Effect::ListAppend { var } => format!("ListAppend({var})"),
            Effect::ListRemoveAt { var } => format!("ListRemoveAt({var})"),
            Effect::Spawn {
                entity_type,
                entity_id_source,
                ..
            } => {
                format!("Spawn({entity_type}, {entity_id_source})")
            }
        })
        .collect::<Vec<_>>()
        .join(" /\\ ")
}

fn validate(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    // 1. Initial state must be in the states list.
    if !automaton
        .automaton
        .states
        .contains(&automaton.automaton.initial)
    {
        return Err(AutomatonParseError::Validation(format!(
            "initial state '{}' is not in states list",
            automaton.automaton.initial
        )));
    }

    // 2. All `from` and `to` states in actions must be declared states.
    for action in &automaton.actions {
        for from in &action.from {
            if !automaton.automaton.states.contains(from) {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' references undeclared from-state '{from}'",
                    action.name
                )));
            }
        }
        if let Some(to) = &action.to
            && !automaton.automaton.states.contains(to)
        {
            return Err(AutomatonParseError::Validation(format!(
                "action '{}' references undeclared to-state '{to}'",
                action.name
            )));
        }
    }

    // 3. Validate WASM integrations.
    let action_names: Vec<&str> = automaton.actions.iter().map(|a| a.name.as_str()).collect();
    for ig in &automaton.integrations {
        if ig.integration_type == "wasm" {
            if ig.module.is_none() {
                return Err(AutomatonParseError::Validation(format!(
                    "integration '{}' is type 'wasm' but missing 'module' field",
                    ig.name
                )));
            }
            if let Some(ref cb) = ig.on_success
                && !action_names.contains(&cb.as_str())
            {
                return Err(AutomatonParseError::Validation(format!(
                    "integration '{}' on_success references unknown action '{cb}'",
                    ig.name
                )));
            }
            if let Some(ref cb) = ig.on_failure
                && !action_names.contains(&cb.as_str())
            {
                return Err(AutomatonParseError::Validation(format!(
                    "integration '{}' on_failure references unknown action '{cb}'",
                    ig.name
                )));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
#[path = "parser_test.rs"]
mod tests;
