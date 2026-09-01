//! Absence-safety checks for nullable action parameters.

use std::collections::BTreeSet;

use crate::automaton::{Action, Effect, Guard, TriggerGuard};

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
                    entity_id_source,
                    copy_fields,
                    ..
                } => {
                    if entity_id_source == name {
                        consumers.insert("spawn identity".to_string());
                    }
                    if copy_fields
                        .as_ref()
                        .is_some_and(|fields| fields.iter().any(|field| field == name))
                    {
                        consumers.insert("spawn copy_fields".to_string());
                    }
                }
                _ => {}
            }
        }

        for trigger in &action.triggers {
            if trigger.params_from.values().any(|source| source == name) {
                consumers.insert("required trigger mapping".to_string());
            }
            if trigger
                .guard
                .as_ref()
                .is_some_and(|guard| trigger_guard_consumes(guard, name))
            {
                consumers.insert("guard".to_string());
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

fn trigger_guard_consumes(guard: &TriggerGuard, parameter: &str) -> bool {
    match guard {
        TriggerGuard::CrossEntityStateIn {
            entity_id_source, ..
        } => entity_id_source == parameter,
        TriggerGuard::FieldEquals { field, .. }
        | TriggerGuard::FieldIn { field, .. }
        | TriggerGuard::BoolTrue { field }
        | TriggerGuard::BoolFalse { field } => field == parameter,
        TriggerGuard::StateIn { .. } => false,
        TriggerGuard::AllOf { guards } | TriggerGuard::AnyOf { guards } => guards
            .iter()
            .any(|child| trigger_guard_consumes(child, parameter)),
        TriggerGuard::Not { guard } => trigger_guard_consumes(guard, parameter),
    }
}

fn references_parameter(value: &str, parameter: &str) -> bool {
    value == parameter
        || value.contains(&format!("{{{parameter}}}"))
        || value.contains(&format!("${{{parameter}}}"))
}
