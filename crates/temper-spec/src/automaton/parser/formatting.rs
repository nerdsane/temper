use crate::automaton::types::{Effect, Guard};

pub(super) fn format_guards(guards: &[Guard]) -> String {
    guards
        .iter()
        .map(|guard| match guard {
            Guard::StateIn { values } => format!(
                "status \\in {{{}}}",
                values
                    .iter()
                    .map(|state| format!("\"{state}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
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
                forbidden_status,
                ..
            } => {
                let set = |statuses: &[String]| {
                    statuses
                        .iter()
                        .map(|status| format!("\"{status}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let mut conjuncts = Vec::new();
                if !required_status.is_empty() {
                    conjuncts.push(format!(
                        "{entity_type}[{entity_id_source}].status \\in {{{}}}",
                        set(required_status)
                    ));
                }
                if !forbidden_status.is_empty() {
                    conjuncts.push(format!(
                        "{entity_type}[{entity_id_source}].status \\notin {{{}}}",
                        set(forbidden_status)
                    ));
                }
                if conjuncts.is_empty() {
                    "TRUE".to_string()
                } else {
                    conjuncts.join(" /\\ ")
                }
            }
            Guard::ReferenceEquals { reference, param } => format!("{reference} = {param}"),
        })
        .collect::<Vec<_>>()
        .join(" /\\ ")
}

pub(super) fn format_effects(effects: &[Effect]) -> String {
    effects
        .iter()
        .map(|effect| match effect {
            Effect::Increment { var, amount } => match amount {
                Some(amount) => format!("{var}' = {var} + {amount}"),
                None => format!("{var}' = {var} + 1"),
            },
            Effect::Decrement { var, amount } => match amount {
                Some(amount) => format!("{var}' = {var} - {amount}"),
                None => format!("{var}' = {var} - 1"),
            },
            Effect::SetCounterFromParam { var, param } => format!("{var}' = {param}"),
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
            Effect::ScheduleAt { action, field } => format!("ScheduleAt(\"{action}\", {field})"),
            Effect::Spawn {
                entity_type,
                entity_id_source,
                ..
            } => format!("Spawn({entity_type}, {entity_id_source})"),
        })
        .collect::<Vec<_>>()
        .join(" /\\ ")
}
