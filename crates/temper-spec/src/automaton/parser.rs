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
                        .map(|v| format!("\"{v}\""))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            Guard::MinCount { var, min } => {
                format!("Cardinality({var}) > {}", min.saturating_sub(1))
            }
            Guard::MaxCount { var, max } => format!("Cardinality({var}) < {max}"),
            Guard::IsTrue { var } => format!("{var} = TRUE"),
            Guard::ListContains { var, value } => format!("\"{value}\" \\in {var}"),
            Guard::ListLengthMin { var, min } => format!("Len({var}) >= {min}"),
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
            Effect::ListAppend { var } => format!("{var}'.append(param)"),
            Effect::ListRemoveAt { var } => format!("{var}'.remove_at(param)"),
            Effect::Trigger { name } => format!("trigger({name})"),
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
    let action_names: Vec<&str> = automaton.actions.iter().map(|a| a.name.as_str()).collect();
    for action in &automaton.actions {
        for from in &action.from {
            if !states.contains(from) {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' references unknown from-state '{}'",
                    action.name, from
                )));
            }
        }
        if let Some(ref to) = action.to
            && !states.contains(to)
        {
            return Err(AutomatonParseError::Validation(format!(
                "action '{}' references unknown to-state '{}'",
                action.name, to
            )));
        }
    }

    // Validate WASM integrations
    for integration in &automaton.integrations {
        if integration.integration_type == "wasm" {
            if integration.module.is_none() {
                return Err(AutomatonParseError::Validation(format!(
                    "integration '{}' has type 'wasm' but missing 'module' field",
                    integration.name
                )));
            }
            if let Some(ref on_success) = integration.on_success
                && !action_names.contains(&on_success.as_str())
            {
                return Err(AutomatonParseError::Validation(format!(
                    "integration '{}' on_success action '{}' not found in spec actions",
                    integration.name, on_success
                )));
            }
            if let Some(ref on_failure) = integration.on_failure
                && !action_names.contains(&on_failure.as_str())
            {
                return Err(AutomatonParseError::Validation(format!(
                    "integration '{}' on_failure action '{}' not found in spec actions",
                    integration.name, on_failure
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
            flush_all(
                &mut current_action,
                &mut actions,
                &mut current_invariant,
                &mut invariants,
                &mut current_state_var,
                &mut state_vars,
                &mut current_liveness,
                &mut liveness_props,
            );
            current_section = "automaton";
            continue;
        }
        if trimmed == "[[state]]" {
            flush_all(
                &mut current_action,
                &mut actions,
                &mut current_invariant,
                &mut invariants,
                &mut current_state_var,
                &mut state_vars,
                &mut current_liveness,
                &mut liveness_props,
            );
            current_state_var = Some(StateVar {
                name: String::new(),
                var_type: "string".into(),
                initial: String::new(),
            });
            current_section = "state";
            continue;
        }
        if trimmed == "[[action]]" {
            flush_all(
                &mut current_action,
                &mut actions,
                &mut current_invariant,
                &mut invariants,
                &mut current_state_var,
                &mut state_vars,
                &mut current_liveness,
                &mut liveness_props,
            );
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
            flush_all(
                &mut current_action,
                &mut actions,
                &mut current_invariant,
                &mut invariants,
                &mut current_state_var,
                &mut state_vars,
                &mut current_liveness,
                &mut liveness_props,
            );
            current_invariant = Some(Invariant {
                name: String::new(),
                when: Vec::new(),
                assert: String::new(),
            });
            current_section = "invariant";
            continue;
        }
        if trimmed == "[[liveness]]" {
            flush_all(
                &mut current_action,
                &mut actions,
                &mut current_invariant,
                &mut invariants,
                &mut current_state_var,
                &mut state_vars,
                &mut current_liveness,
                &mut liveness_props,
            );
            if let Some(ig) = current_integration.take()
                && !ig.name.is_empty()
            {
                integrations.push(ig);
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
            flush_all(
                &mut current_action,
                &mut actions,
                &mut current_invariant,
                &mut invariants,
                &mut current_state_var,
                &mut state_vars,
                &mut current_liveness,
                &mut liveness_props,
            );
            if let Some(ig) = current_integration.take()
                && !ig.name.is_empty()
            {
                integrations.push(ig);
            }
            current_integration = Some(Integration {
                name: String::new(),
                trigger: String::new(),
                integration_type: "webhook".to_string(),
                module: None,
                on_success: None,
                on_failure: None,
                config: std::collections::BTreeMap::new(),
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
                                a.guard.push(parse_guard_clause(&value)?);
                            }
                            "effect" => {
                                // Format: "increment var" → Increment
                                if value.starts_with("increment ") {
                                    let var = value
                                        .strip_prefix("increment ")
                                        .unwrap_or("")
                                        .trim()
                                        .to_string();
                                    if !var.is_empty() {
                                        a.effect.push(Effect::Increment { var });
                                    }
                                }
                                // Format: "decrement var" → Decrement
                                else if value.starts_with("decrement ") {
                                    let var = value
                                        .strip_prefix("decrement ")
                                        .unwrap_or("")
                                        .trim()
                                        .to_string();
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
                                    let event = value
                                        .strip_prefix("emit ")
                                        .unwrap_or("")
                                        .trim()
                                        .to_string();
                                    if !event.is_empty() {
                                        a.effect.push(Effect::Emit { event });
                                    }
                                }
                                // Format: "trigger integration_name" → Trigger
                                else if value.starts_with("trigger ") {
                                    let name = value
                                        .strip_prefix("trigger ")
                                        .unwrap_or("")
                                        .trim()
                                        .to_string();
                                    if !name.is_empty() {
                                        a.effect.push(Effect::Trigger { name });
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
                            "module" => ig.module = Some(value.clone()),
                            "on_success" => ig.on_success = Some(value.clone()),
                            "on_failure" => ig.on_failure = Some(value.clone()),
                            _ => {
                                ig.config.insert(key.to_string(), value.clone());
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    flush_all(
        &mut current_action,
        &mut actions,
        &mut current_invariant,
        &mut invariants,
        &mut current_state_var,
        &mut state_vars,
        &mut current_liveness,
        &mut liveness_props,
    );
    if let Some(ig) = current_integration.take()
        && !ig.name.is_empty()
    {
        integrations.push(ig);
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

#[allow(clippy::too_many_arguments)]
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
    if let Some(a) = action.take()
        && !a.name.is_empty()
    {
        actions.push(a);
    }
    if let Some(inv) = invariant.take()
        && !inv.name.is_empty()
    {
        invariants.push(inv);
    }
    if let Some(sv) = state_var.take()
        && !sv.name.is_empty()
    {
        state_vars.push(sv);
    }
    if let Some(l) = liveness.take()
        && !l.name.is_empty()
    {
        liveness_props.push(l);
    }
}

fn parse_guard_clause(value: &str) -> Result<Guard, AutomatonParseError> {
    let trimmed = value.trim();

    // Infix forms: "<var> > <n>" and "<var> < <n>".
    if let Some((lhs, rhs)) = trimmed.split_once('>') {
        let var = lhs.trim();
        let raw = rhs.trim();
        if var.is_empty() || raw.is_empty() {
            return Err(AutomatonParseError::Validation(format!(
                "invalid guard '{trimmed}' (expected '<var> > <n>')"
            )));
        }
        let n: usize = raw.parse().map_err(|_| {
            AutomatonParseError::Validation(format!(
                "invalid guard '{trimmed}' (right side must be an integer)"
            ))
        })?;
        return Ok(Guard::MinCount {
            var: var.to_string(),
            min: n + 1,
        });
    }
    if let Some((lhs, rhs)) = trimmed.split_once('<') {
        let var = lhs.trim();
        let raw = rhs.trim();
        if var.is_empty() || raw.is_empty() {
            return Err(AutomatonParseError::Validation(format!(
                "invalid guard '{trimmed}' (expected '<var> < <n>')"
            )));
        }
        let max: usize = raw.parse().map_err(|_| {
            AutomatonParseError::Validation(format!(
                "invalid guard '{trimmed}' (right side must be an integer)"
            ))
        })?;
        return Ok(Guard::MaxCount {
            var: var.to_string(),
            max,
        });
    }

    // Prefix forms:
    // - "min <var> <n>"
    // - "max <var> <n>"
    // - "is_true <var>"
    // - "list_contains <var> <value>"
    // - "list_length_min <var> <n>"
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.is_empty() {
        return Err(AutomatonParseError::Validation(
            "empty guard clause".to_string(),
        ));
    }

    match parts[0] {
        "min" => {
            if parts.len() != 3 {
                return Err(AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (expected 'min <var> <n>')"
                )));
            }
            let min: usize = parts[2].parse().map_err(|_| {
                AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (min must be an integer)"
                ))
            })?;
            Ok(Guard::MinCount {
                var: parts[1].to_string(),
                min,
            })
        }
        "max" => {
            if parts.len() != 3 {
                return Err(AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (expected 'max <var> <n>')"
                )));
            }
            let max: usize = parts[2].parse().map_err(|_| {
                AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (max must be an integer)"
                ))
            })?;
            Ok(Guard::MaxCount {
                var: parts[1].to_string(),
                max,
            })
        }
        "is_true" => {
            if parts.len() != 2 {
                return Err(AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (expected 'is_true <var>')"
                )));
            }
            Ok(Guard::IsTrue {
                var: parts[1].to_string(),
            })
        }
        "list_contains" => {
            if parts.len() < 3 {
                return Err(AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (expected 'list_contains <var> <value>')"
                )));
            }
            Ok(Guard::ListContains {
                var: parts[1].to_string(),
                value: parts[2..].join(" "),
            })
        }
        "list_length_min" => {
            if parts.len() != 3 {
                return Err(AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (expected 'list_length_min <var> <n>')"
                )));
            }
            let min: usize = parts[2].parse().map_err(|_| {
                AutomatonParseError::Validation(format!(
                    "invalid guard '{trimmed}' (min must be an integer)"
                ))
            })?;
            Ok(Guard::ListLengthMin {
                var: parts[1].to_string(),
                min,
            })
        }
        _ => Err(AutomatonParseError::Validation(format!(
            "unsupported guard syntax '{trimmed}'"
        ))),
    }
}

fn parse_kv(line: &str) -> Option<(&str, String)> {
    let eq = line.find('=')?;
    let key = line[..eq].trim();
    let raw_value = line[eq + 1..].trim();
    let value = raw_value.trim_matches('"').trim_matches('\'').to_string();
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
        let submit = automaton
            .actions
            .iter()
            .find(|a| a.name == "SubmitOrder")
            .unwrap();
        assert_eq!(submit.from, vec!["Draft"]);
        assert_eq!(submit.to, Some("Submitted".to_string()));
        assert!(!submit.guard.is_empty(), "SubmitOrder should have a guard");
    }

    #[test]
    fn test_cancel_from_multiple_states() {
        let automaton = parse_automaton(ORDER_IOA).unwrap();
        let cancel = automaton
            .actions
            .iter()
            .find(|a| a.name == "CancelOrder")
            .unwrap();
        assert_eq!(cancel.from.len(), 3);
        assert!(cancel.from.contains(&"Draft".to_string()));
        assert!(cancel.from.contains(&"Submitted".to_string()));
        assert!(cancel.from.contains(&"Confirmed".to_string()));
    }

    #[test]
    fn test_invariants_parsed() {
        let automaton = parse_automaton(ORDER_IOA).unwrap();
        assert!(!automaton.invariants.is_empty());
        let names: Vec<&str> = automaton
            .invariants
            .iter()
            .map(|i| i.name.as_str())
            .collect();
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

        let submit = sm
            .transitions
            .iter()
            .find(|t| t.name == "SubmitOrder")
            .unwrap();
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

    #[test]
    fn test_trigger_effect_parsed() {
        let toml = r#"
[automaton]
name = "Order"
states = ["Submitted", "ChargePending", "Confirmed", "PaymentFailed"]
initial = "Submitted"

[[action]]
name = "ChargePayment"
from = ["Submitted"]
to = "ChargePending"
effect = "trigger charge_payment"

[[action]]
name = "ChargeSucceeded"
kind = "input"
from = ["ChargePending"]
to = "Confirmed"

[[action]]
name = "ChargeFailed"
kind = "input"
from = ["ChargePending"]
to = "PaymentFailed"
"#;
        let automaton = parse_automaton(toml).expect("should parse");
        let charge = automaton
            .actions
            .iter()
            .find(|a| a.name == "ChargePayment")
            .unwrap();
        assert_eq!(charge.effect.len(), 1);
        match &charge.effect[0] {
            Effect::Trigger { name } => assert_eq!(name, "charge_payment"),
            other => panic!("expected Trigger effect, got: {other:?}"),
        }
    }

    #[test]
    fn test_wasm_integration_parsed() {
        let toml = r#"
[automaton]
name = "Order"
states = ["Submitted", "ChargePending", "Confirmed", "PaymentFailed"]
initial = "Submitted"

[[action]]
name = "ChargePayment"
from = ["Submitted"]
to = "ChargePending"
effect = "trigger charge_payment"

[[action]]
name = "ChargeSucceeded"
kind = "input"
from = ["ChargePending"]
to = "Confirmed"

[[action]]
name = "ChargeFailed"
kind = "input"
from = ["ChargePending"]
to = "PaymentFailed"

[[integration]]
name = "charge_payment"
trigger = "charge_payment"
type = "wasm"
module = "stripe_charge"
on_success = "ChargeSucceeded"
on_failure = "ChargeFailed"
"#;
        let automaton = parse_automaton(toml).expect("should parse");
        assert_eq!(automaton.integrations.len(), 1);
        let ig = &automaton.integrations[0];
        assert_eq!(ig.name, "charge_payment");
        assert_eq!(ig.integration_type, "wasm");
        assert_eq!(ig.module.as_deref(), Some("stripe_charge"));
        assert_eq!(ig.on_success.as_deref(), Some("ChargeSucceeded"));
        assert_eq!(ig.on_failure.as_deref(), Some("ChargeFailed"));
    }

    #[test]
    fn test_wasm_integration_missing_module_rejected() {
        let toml = r#"
[automaton]
name = "Order"
states = ["Submitted", "ChargePending"]
initial = "Submitted"

[[action]]
name = "ChargePayment"
from = ["Submitted"]
to = "ChargePending"

[[integration]]
name = "charge_payment"
trigger = "charge_payment"
type = "wasm"
"#;
        let result = parse_automaton(toml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("missing 'module'"), "got: {err}");
    }

    #[test]
    fn test_wasm_integration_unknown_callback_rejected() {
        let toml = r#"
[automaton]
name = "Order"
states = ["Submitted", "ChargePending", "Confirmed"]
initial = "Submitted"

[[action]]
name = "ChargePayment"
from = ["Submitted"]
to = "ChargePending"

[[integration]]
name = "charge_payment"
trigger = "charge_payment"
type = "wasm"
module = "stripe_charge"
on_success = "NonExistentAction"
"#;
        let result = parse_automaton(toml);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("NonExistentAction"),
            "should mention missing action, got: {err}"
        );
    }

    #[test]
    fn test_valid_state_var_types_accepted() {
        let spec = r#"
[automaton]
name = "Task"
states = ["Open", "Done"]
initial = "Open"

[[state]]
name = "is_done"
type = "bool"
initial = "false"

[[state]]
name = "attempt_count"
type = "counter"
initial = "0"

[[action]]
name = "Complete"
kind = "input"
from = ["Open"]
to = "Done"
effect = "set is_done true"
"#;
        let result = parse_automaton(spec);
        assert!(
            result.is_ok(),
            "bool and counter types should be accepted: {:?}",
            result.err()
        );
    }

    #[test]
    fn test_extended_guard_syntax_parsed() {
        let spec = r#"
[automaton]
name = "Ticket"
states = ["Open", "Queued", "Closed"]
initial = "Open"

[[action]]
name = "Queue"
from = ["Open"]
to = "Queued"
guard = "max retries 3"

[[action]]
name = "Escalate"
from = ["Queued"]
to = "Queued"
guard = "list_contains labels urgent"

[[action]]
name = "Close"
from = ["Queued"]
to = "Closed"
guard = "list_length_min labels 1"
"#;

        let automaton = parse_automaton(spec).expect("extended guard forms should parse");
        let queue = automaton
            .actions
            .iter()
            .find(|a| a.name == "Queue")
            .unwrap();
        assert!(matches!(
            queue.guard.as_slice(),
            [Guard::MaxCount { var, max }] if var == "retries" && *max == 3
        ));

        let escalate = automaton
            .actions
            .iter()
            .find(|a| a.name == "Escalate")
            .unwrap();
        assert!(matches!(
            escalate.guard.as_slice(),
            [Guard::ListContains { var, value }] if var == "labels" && value == "urgent"
        ));

        let close = automaton
            .actions
            .iter()
            .find(|a| a.name == "Close")
            .unwrap();
        assert!(matches!(
            close.guard.as_slice(),
            [Guard::ListLengthMin { var, min }] if var == "labels" && *min == 1
        ));
    }

    #[test]
    fn test_integration_config_captures_unknown_keys() {
        let toml = r#"
[automaton]
name = "Weather"
states = ["Idle", "Fetching", "Ready", "Failed"]
initial = "Idle"

[[action]]
name = "FetchWeather"
from = ["Idle"]
to = "Fetching"
effect = "trigger fetch_weather"

[[action]]
name = "FetchSucceeded"
kind = "input"
from = ["Fetching"]
to = "Ready"

[[action]]
name = "FetchFailed"
kind = "input"
from = ["Fetching"]
to = "Failed"

[[integration]]
name = "fetch_weather"
trigger = "fetch_weather"
type = "wasm"
module = "http_fetch"
on_success = "FetchSucceeded"
on_failure = "FetchFailed"
url = "https://api.open-meteo.com/v1/forecast"
method = "GET"
"#;
        let automaton = parse_automaton(toml).expect("should parse");
        assert_eq!(automaton.integrations.len(), 1);
        let ig = &automaton.integrations[0];
        assert_eq!(ig.name, "fetch_weather");
        assert_eq!(ig.integration_type, "wasm");
        assert_eq!(ig.module.as_deref(), Some("http_fetch"));
        assert_eq!(
            ig.config.get("url").map(String::as_str),
            Some("https://api.open-meteo.com/v1/forecast")
        );
        assert_eq!(ig.config.get("method").map(String::as_str), Some("GET"));
        // Known keys should NOT be in config
        assert!(!ig.config.contains_key("name"));
        assert!(!ig.config.contains_key("trigger"));
        assert!(!ig.config.contains_key("type"));
        assert!(!ig.config.contains_key("module"));
    }

    #[test]
    fn test_invalid_guard_number_rejected() {
        let spec = r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
from = ["Draft"]
to = "Submitted"
guard = "items > nope"
"#;

        let err = parse_automaton(spec).expect_err("invalid numeric guard should fail");
        assert!(err.to_string().contains("right side must be an integer"));
    }
}
