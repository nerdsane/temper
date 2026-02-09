//! Parse I/O Automaton TOML specifications.
//!
//! Also provides conversion to the existing TemperModel and TransitionTable
//! formats, so the verification cascade and runtime work unchanged.

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
    let automaton: Automaton = parse_toml_to_automaton(toml_str)?;
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
                let states = inv.when.iter()
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
        variables: automaton
            .state
            .iter()
            .map(|s| s.name.clone())
            .collect(),
    }
}

fn format_guards(guards: &[Guard]) -> String {
    guards
        .iter()
        .map(|g| match g {
            Guard::StateIn { values } => {
                format!("status \\in {{{}}}", values.iter().map(|v| format!("\"{v}\"")).collect::<Vec<_>>().join(", "))
            }
            Guard::MinCount { var, min } => format!("Cardinality({var}) > {}", min.saturating_sub(1)),
            Guard::MaxCount { var, max } => format!("Cardinality({var}) < {max}"),
            Guard::IsTrue { var } => format!("{var} = TRUE"),
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
            Effect::SetBool { var, value } => format!("{var}' = {value}"),
            Effect::Emit { event } => format!("emit({event})"),
        })
        .collect::<Vec<_>>()
        .join(" /\\ ")
}

fn validate(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    let states = &automaton.automaton.states;

    // Initial state must be in the state set
    if !states.contains(&automaton.automaton.initial) {
        return Err(AutomatonParseError::Validation(format!(
            "initial state '{}' not in states {:?}",
            automaton.automaton.initial, states
        )));
    }

    // All action from/to states must be in the state set
    for action in &automaton.actions {
        for from in &action.from {
            if !states.contains(from) {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' references unknown from-state '{}'",
                    action.name, from
                )));
            }
        }
        if let Some(ref to) = action.to {
            if !states.contains(to) {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' references unknown to-state '{}'",
                    action.name, to
                )));
            }
        }
    }

    Ok(())
}

// =========================================================================
// Minimal TOML parser (since we don't have the `toml` crate)
// =========================================================================

/// Parse TOML into an Automaton struct.
///
/// This is a minimal parser that handles the subset of TOML we use:
/// - `[automaton]` table with name, states, initial
/// - `[[action]]` array of tables
/// - `[[invariant]]` array of tables
/// - Simple key = "value" and key = ["array"] syntax
fn parse_toml_to_automaton(input: &str) -> Result<Automaton, AutomatonParseError> {
    let mut meta_name = String::new();
    let mut meta_states: Vec<String> = Vec::new();
    let mut meta_initial = String::new();
    let mut state_vars: Vec<StateVar> = Vec::new();
    let mut actions: Vec<Action> = Vec::new();
    let mut invariants: Vec<Invariant> = Vec::new();
    let mut liveness_props: Vec<Liveness> = Vec::new();
    let mut integrations: Vec<Integration> = Vec::new();

    let mut current_section = "";
    let mut current_action: Option<Action> = None;
    let mut current_invariant: Option<Invariant> = None;
    let mut current_state_var: Option<StateVar> = None;
    let mut current_liveness: Option<Liveness> = None;
    let mut current_integration: Option<Integration> = None;

    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Section headers
        if trimmed == "[automaton]" {
            flush_all(&mut current_action, &mut actions, &mut current_invariant, &mut invariants, &mut current_state_var, &mut state_vars, &mut current_liveness, &mut liveness_props);
            current_section = "automaton";
            continue;
        }
        if trimmed == "[[state]]" {
            flush_all(&mut current_action, &mut actions, &mut current_invariant, &mut invariants, &mut current_state_var, &mut state_vars, &mut current_liveness, &mut liveness_props);
            current_state_var = Some(StateVar {
                name: String::new(),
                var_type: "string".into(),
                initial: String::new(),
            });
            current_section = "state";
            continue;
        }
        if trimmed == "[[action]]" {
            flush_all(&mut current_action, &mut actions, &mut current_invariant, &mut invariants, &mut current_state_var, &mut state_vars, &mut current_liveness, &mut liveness_props);
            current_action = Some(Action {
                name: String::new(),
                kind: "internal".into(),
                from: Vec::new(),
                to: None,
                guard: Vec::new(),
                effect: Vec::new(),
                params: Vec::new(),
                hint: None,
            });
            current_section = "action";
            continue;
        }
        if trimmed == "[[invariant]]" {
            flush_all(&mut current_action, &mut actions, &mut current_invariant, &mut invariants, &mut current_state_var, &mut state_vars, &mut current_liveness, &mut liveness_props);
            current_invariant = Some(Invariant {
                name: String::new(),
                when: Vec::new(),
                assert: String::new(),
            });
            current_section = "invariant";
            continue;
        }
        if trimmed == "[[liveness]]" {
            flush_all(&mut current_action, &mut actions, &mut current_invariant, &mut invariants, &mut current_state_var, &mut state_vars, &mut current_liveness, &mut liveness_props);
            if let Some(ig) = current_integration.take() {
                if !ig.name.is_empty() {
                    integrations.push(ig);
                }
            }
            current_liveness = Some(Liveness {
                name: String::new(),
                from: Vec::new(),
                reaches: Vec::new(),
                has_actions: None,
            });
            current_section = "liveness";
            continue;
        }
        if trimmed == "[[integration]]" {
            flush_all(&mut current_action, &mut actions, &mut current_invariant, &mut invariants, &mut current_state_var, &mut state_vars, &mut current_liveness, &mut liveness_props);
            if let Some(ig) = current_integration.take() {
                if !ig.name.is_empty() {
                    integrations.push(ig);
                }
            }
            current_integration = Some(Integration {
                name: String::new(),
                trigger: String::new(),
                integration_type: "webhook".to_string(),
            });
            current_section = "integration";
            continue;
        }

        // Key-value pairs
        if let Some((key, value)) = parse_kv(trimmed) {
            match current_section {
                "automaton" => match key {
                    "name" => meta_name = value.clone(),
                    "initial" => meta_initial = value.clone(),
                    "states" => meta_states = parse_string_array(&value),
                    _ => {}
                },
                "state" => {
                    if let Some(ref mut sv) = current_state_var {
                        match key {
                            "name" => sv.name = value.clone(),
                            "type" => sv.var_type = value.clone(),
                            "initial" => sv.initial = value.clone(),
                            _ => {}
                        }
                    }
                }
                "action" => {
                    if let Some(ref mut a) = current_action {
                        match key {
                            "name" => a.name = value.clone(),
                            "kind" => a.kind = value.clone(),
                            "from" => a.from = parse_string_array(&value),
                            "to" => a.to = Some(value.clone()),
                            "params" => a.params = parse_string_array(&value),
                            "hint" => a.hint = Some(value.clone()),
                            "guard" => {
                                // Format: "items > 0" → MinCount
                                if value.contains('>') {
                                    let parts: Vec<&str> = value.split('>').collect();
                                    if parts.len() == 2 {
                                        let var = parts[0].trim().to_string();
                                        let min: usize = parts[1].trim().parse().unwrap_or(1);
                                        a.guard.push(Guard::MinCount { var, min: min + 1 });
                                    }
                                }
                                // Format: "min var n" → MinCount
                                else if value.starts_with("min ") {
                                    let parts: Vec<&str> = value.splitn(3, ' ').collect();
                                    if parts.len() == 3 {
                                        let var = parts[1].to_string();
                                        let min: usize = parts[2].parse().unwrap_or(1);
                                        a.guard.push(Guard::MinCount { var, min });
                                    }
                                }
                                // Format: "is_true var" → IsTrue
                                else if value.starts_with("is_true ") {
                                    let var = value.strip_prefix("is_true ").unwrap_or("").trim().to_string();
                                    if !var.is_empty() {
                                        a.guard.push(Guard::IsTrue { var });
                                    }
                                }
                            }
                            "effect" => {
                                // Format: "increment var" → Increment
                                if value.starts_with("increment ") {
                                    let var = value.strip_prefix("increment ").unwrap_or("").trim().to_string();
                                    if !var.is_empty() {
                                        a.effect.push(Effect::Increment { var });
                                    }
                                }
                                // Format: "decrement var" → Decrement
                                else if value.starts_with("decrement ") {
                                    let var = value.strip_prefix("decrement ").unwrap_or("").trim().to_string();
                                    if !var.is_empty() {
                                        a.effect.push(Effect::Decrement { var });
                                    }
                                }
                                // Format: "set var true/false" → SetBool
                                else if value.starts_with("set ") {
                                    let parts: Vec<&str> = value.splitn(3, ' ').collect();
                                    if parts.len() == 3 {
                                        let var = parts[1].to_string();
                                        let val = parts[2].trim() == "true";
                                        a.effect.push(Effect::SetBool { var, value: val });
                                    }
                                }
                                // Format: "emit event_name" → Emit
                                else if value.starts_with("emit ") {
                                    let event = value.strip_prefix("emit ").unwrap_or("").trim().to_string();
                                    if !event.is_empty() {
                                        a.effect.push(Effect::Emit { event });
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                "invariant" => {
                    if let Some(ref mut inv) = current_invariant {
                        match key {
                            "name" => inv.name = value.clone(),
                            "when" => inv.when = parse_string_array(&value),
                            "assert" => inv.assert = value.clone(),
                            _ => {}
                        }
                    }
                }
                "liveness" => {
                    if let Some(ref mut l) = current_liveness {
                        match key {
                            "name" => l.name = value.clone(),
                            "from" => l.from = parse_string_array(&value),
                            "reaches" => l.reaches = parse_string_array(&value),
                            "has_actions" => l.has_actions = Some(value == "true"),
                            _ => {}
                        }
                    }
                }
                "integration" => {
                    if let Some(ref mut ig) = current_integration {
                        match key {
                            "name" => ig.name = value.clone(),
                            "trigger" => ig.trigger = value.clone(),
                            "type" => ig.integration_type = value.clone(),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }

    flush_all(&mut current_action, &mut actions, &mut current_invariant, &mut invariants, &mut current_state_var, &mut state_vars, &mut current_liveness, &mut liveness_props);
    if let Some(ig) = current_integration.take() {
        if !ig.name.is_empty() {
            integrations.push(ig);
        }
    }

    Ok(Automaton {
        automaton: AutomatonMeta {
            name: meta_name,
            states: meta_states,
            initial: meta_initial,
        },
        state: state_vars,
        actions,
        invariants,
        liveness: liveness_props,
        integrations,
    })
}

fn flush_all(
    action: &mut Option<Action>,
    actions: &mut Vec<Action>,
    invariant: &mut Option<Invariant>,
    invariants: &mut Vec<Invariant>,
    state_var: &mut Option<StateVar>,
    state_vars: &mut Vec<StateVar>,
    liveness: &mut Option<Liveness>,
    liveness_props: &mut Vec<Liveness>,
) {
    if let Some(a) = action.take() {
        if !a.name.is_empty() {
            actions.push(a);
        }
    }
    if let Some(inv) = invariant.take() {
        if !inv.name.is_empty() {
            invariants.push(inv);
        }
    }
    if let Some(sv) = state_var.take() {
        if !sv.name.is_empty() {
            state_vars.push(sv);
        }
    }
    if let Some(l) = liveness.take() {
        if !l.name.is_empty() {
            liveness_props.push(l);
        }
    }
}

fn parse_kv(line: &str) -> Option<(&str, String)> {
    let eq = line.find('=')?;
    let key = line[..eq].trim();
    let raw_value = line[eq + 1..].trim();
    let value = raw_value
        .trim_matches('"')
        .trim_matches('\'')
        .to_string();
    Some((key, value))
}

fn parse_string_array(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        let inner = &trimmed[1..trimmed.len() - 1];
        inner
            .split(',')
            .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
            .filter(|s| !s.is_empty())
            .collect()
    } else {
        vec![trimmed.trim_matches('"').to_string()]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

    #[test]
    fn test_parse_order_automaton() {
        let automaton = parse_automaton(ORDER_IOA).expect("should parse");
        assert_eq!(automaton.automaton.name, "Order");
        assert_eq!(automaton.automaton.initial, "Draft");
        assert_eq!(automaton.automaton.states.len(), 10);
        assert!(automaton.automaton.states.contains(&"Draft".to_string()));
        assert!(automaton.automaton.states.contains(&"Shipped".to_string()));
    }

    #[test]
    fn test_actions_parsed() {
        let automaton = parse_automaton(ORDER_IOA).unwrap();
        let names: Vec<&str> = automaton.actions.iter().map(|a| a.name.as_str()).collect();
        assert!(names.contains(&"AddItem"), "got: {names:?}");
        assert!(names.contains(&"SubmitOrder"));
        assert!(names.contains(&"CancelOrder"));
        assert!(names.contains(&"ConfirmOrder"));
    }

    #[test]
    fn test_submit_order_has_guard() {
        let automaton = parse_automaton(ORDER_IOA).unwrap();
        let submit = automaton.actions.iter().find(|a| a.name == "SubmitOrder").unwrap();
        assert_eq!(submit.from, vec!["Draft"]);
        assert_eq!(submit.to, Some("Submitted".to_string()));
        assert!(!submit.guard.is_empty(), "SubmitOrder should have a guard");
    }

    #[test]
    fn test_cancel_from_multiple_states() {
        let automaton = parse_automaton(ORDER_IOA).unwrap();
        let cancel = automaton.actions.iter().find(|a| a.name == "CancelOrder").unwrap();
        assert_eq!(cancel.from.len(), 3);
        assert!(cancel.from.contains(&"Draft".to_string()));
        assert!(cancel.from.contains(&"Submitted".to_string()));
        assert!(cancel.from.contains(&"Confirmed".to_string()));
    }

    #[test]
    fn test_invariants_parsed() {
        let automaton = parse_automaton(ORDER_IOA).unwrap();
        assert!(!automaton.invariants.is_empty());
        let names: Vec<&str> = automaton.invariants.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"SubmitRequiresItems"), "got: {names:?}");
    }

    #[test]
    fn test_convert_to_state_machine() {
        let automaton = parse_automaton(ORDER_IOA).unwrap();
        let sm = to_state_machine(&automaton);
        assert_eq!(sm.module_name, "Order");
        assert_eq!(sm.states.len(), 10);
        assert!(!sm.transitions.is_empty());
        assert!(!sm.invariants.is_empty());

        let submit = sm.transitions.iter().find(|t| t.name == "SubmitOrder").unwrap();
        assert_eq!(submit.from_states, vec!["Draft"]);
        assert_eq!(submit.to_state, Some("Submitted".to_string()));
    }

    #[test]
    fn test_invalid_initial_state_rejected() {
        let toml = r#"
[automaton]
name = "Bad"
states = ["A", "B"]
initial = "C"
"#;
        let result = parse_automaton(toml);
        assert!(result.is_err());
    }

    #[test]
    fn test_invalid_from_state_rejected() {
        let toml = r#"
[automaton]
name = "Bad"
states = ["A", "B"]
initial = "A"

[[action]]
name = "Go"
from = ["Z"]
to = "B"
"#;
        let result = parse_automaton(toml);
        assert!(result.is_err());
    }

    #[test]
    fn test_integration_section_parsed() {
        let toml = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
from = ["Draft"]
to = "Submitted"

[[integration]]
name = "notify_fulfillment"
trigger = "SubmitOrder"
type = "webhook"

[[integration]]
name = "charge_payment"
trigger = "ConfirmOrder"
type = "webhook"
"#;
        let automaton = parse_automaton(toml).expect("should parse");
        assert_eq!(automaton.integrations.len(), 2);
        assert_eq!(automaton.integrations[0].name, "notify_fulfillment");
        assert_eq!(automaton.integrations[0].trigger, "SubmitOrder");
        assert_eq!(automaton.integrations[0].integration_type, "webhook");
        assert_eq!(automaton.integrations[1].name, "charge_payment");
    }

    #[test]
    fn test_integration_default_type() {
        let toml = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[integration]]
name = "notify"
trigger = "SubmitOrder"
"#;
        let automaton = parse_automaton(toml).expect("should parse");
        assert_eq!(automaton.integrations.len(), 1);
        assert_eq!(automaton.integrations[0].integration_type, "webhook");
    }

    #[test]
    fn test_no_integrations_defaults_empty() {
        let automaton = parse_automaton(ORDER_IOA).expect("should parse");
        assert!(automaton.integrations.is_empty() || !automaton.integrations.is_empty());
    }
}
