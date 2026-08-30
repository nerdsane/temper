//! Absence-safety checks for nullable action parameters.

use std::collections::BTreeSet;

use crate::automaton::{Action, Effect, Guard};

use super::LintFinding;

pub(super) fn lint_nullable_action_parameter_consumers(
    action: &Action,
    findings: &mut Vec<LintFinding>,
) {
    for param in action.params.iter().filter(|param| param.nullable()) {
        let name = param.name();
        let mut consumers = BTreeSet::new();

        for guard in &action.guard {
            match guard {
                Guard::ListContains { value, .. } if references_parameter(value, name) => {
                    consumers.insert("guard".to_string());
                }
                Guard::CrossEntityState {
                    entity_id_source, ..
                } if entity_id_source == name => {
                    consumers.insert("guard".to_string());
                }
                Guard::ReferenceEquals { param, .. } if param == name => {
                    consumers.insert("guard".to_string());
                }
                _ => {}
            }
        }

        for effect in &action.effect {
            match effect {
                Effect::Increment {
                    amount: Some(amount),
                    ..
                }
                | Effect::Decrement {
                    amount: Some(amount),
                    ..
                } if amount == name => {
                    consumers.insert("counter effect".to_string());
                }
                Effect::SetCounterFromParam { param, .. } if param == name => {
                    consumers.insert("counter effect".to_string());
                }
                Effect::ListAppend { var } if var == name => {
                    consumers.insert("list effect".to_string());
                }
                Effect::ListRemoveAt { var } if format!("{var}_index") == name => {
                    consumers.insert("list effect".to_string());
                }
                Effect::Spawn {
                    entity_id_source, ..
                } if entity_id_source == name => {
                    consumers.insert("spawn identity".to_string());
                }
                _ => {}
            }
        }

        for trigger in &action.triggers {
            if trigger.params_from.values().any(|source| source == name) {
                consumers.insert("required trigger mapping".to_string());
            }
            let template_consumed = trigger
                .config
                .values()
                .chain(trigger.headers.values())
                .chain(trigger.url.iter())
                .chain(trigger.body_template.iter())
                .any(|template| references_parameter(template, name));
            if template_consumed {
                consumers.insert("template substitution".to_string());
            }
        }

        for consumer in consumers {
            findings.push(LintFinding::error(
                "nullable_action_parameter_consumed",
                format!(
                    "action '{}' nullable parameter '{}' is consumed by {}; absence semantics are not defined",
                    action.name, name, consumer
                ),
            ));
        }
    }
}

fn references_parameter(value: &str, parameter: &str) -> bool {
    value == parameter
        || value.contains(&format!("{{{parameter}}}"))
        || value.contains(&format!("${{{parameter}}}"))
}
