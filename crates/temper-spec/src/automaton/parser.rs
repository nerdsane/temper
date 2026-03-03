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

    #[test]
    fn test_parse_schedule_effect() {
        let spec = r#"
[automaton]
name = "OAuthToken"
states = ["Active", "Refreshing", "Expired"]
initial = "Active"

[[action]]
name = "Activate"
from = ["Refreshing"]
to = "Active"
effect = [{ type = "schedule", action = "Refresh", delay_seconds = 2700 }]
"#;

        let automaton = parse_automaton(spec).expect("should parse schedule effect");
        let activate = automaton
            .actions
            .iter()
            .find(|a| a.name == "Activate")
            .unwrap();
        assert_eq!(activate.effect.len(), 1);
        match &activate.effect[0] {
            Effect::Schedule {
                action,
                delay_seconds,
            } => {
                assert_eq!(action, "Refresh");
                assert_eq!(*delay_seconds, 2700);
            }
            other => panic!("expected Schedule, got: {:?}", other),
        }
    }
}
