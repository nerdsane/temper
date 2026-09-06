//! Validate parameter constraints before a specification is installed.
use super::{ActionConstraint, ActionParam, Automaton, parser::AutomatonParseError};
use std::collections::BTreeSet;

fn category(kind: &str) -> &str {
    match kind {
        "counter" | "int" | "integer" => "integer",
        "status" => "string",
        other => other,
    }
}

pub(super) fn validate(automaton: &Automaton) -> Result<(), AutomatonParseError> {
    let contracted = automaton.automaton.strict_action_params
        || automaton
            .actions
            .iter()
            .any(|action| !action.constraints.is_empty());
    let mut names = BTreeSet::new();
    if contracted {
        for action in &automaton.actions {
            if !names.insert(&action.name) {
                return Err(AutomatonParseError::Validation(format!(
                    "action '{}' has more than one parameter contract",
                    action.name
                )));
            }
        }
    }
    for field in &automaton.state {
        if automaton.automaton.strict_action_params
            && field.var_type == "counter"
            && field.initial.trim().parse::<usize>().is_err()
        {
            return Err(AutomatonParseError::Validation(format!(
                "counter '{}' requires a natural-number initial value",
                field.name
            )));
        }
        if automaton.automaton.strict_action_params
            && matches!(field.var_type.as_str(), "int" | "integer")
            && field.initial.trim().parse::<i64>().is_err()
        {
            return Err(AutomatonParseError::Validation(format!(
                "integer '{}' requires a signed 64-bit initial value",
                field.name
            )));
        }
    }
    for action in &automaton.actions {
        for constraint in &action.constraints {
            let fail = |reason: &str| {
                AutomatonParseError::Validation(format!(
                    "action '{}' constraint for '{}': {reason}",
                    action.name,
                    constraint.param()
                ))
            };
            let param = action
                .params
                .iter()
                .find(|param| param.name() == constraint.param())
                .ok_or_else(|| fail("parameter is undeclared"))?;
            let field_type = if let Some(name) = constraint.field() {
                Some(
                    if let Some(field) = automaton.state.iter().find(|field| field.name == name) {
                        category(&field.var_type)
                    } else if matches!(name, "Id" | "id") {
                        "string"
                    } else {
                        return Err(fail("field is undeclared"));
                    },
                )
            } else {
                None
            };
            let required_type = match constraint {
                ActionConstraint::ParamGreaterThanField { .. } => {
                    if field_type != Some("integer") {
                        return Err(fail("greater-than requires an integer field"));
                    }
                    Some("integer")
                }
                ActionConstraint::ParamNonempty { .. } => Some("string"),
                _ => field_type,
            };
            if let ActionParam::Typed { param_type, .. } = param
                && required_type.is_some_and(|expected| category(param_type) != expected)
            {
                return Err(fail(
                    "declared parameter type does not match the constraint",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::parse_automaton;
    fn spec(field_type: &str, param: &str, kind: &str) -> String {
        format!(
            r#"
[automaton]
name = "Counter"
states = ["Ready"]
initial = "Ready"
strict_action_params = true
[[state]]
name = "sequence"
type = "{field_type}"
initial = "0"
[[action]]
name = "Observe"
from = ["Ready"]
params = [{param}]
[[action.constraints]]
kind = "{kind}"
param = "value"
field = "sequence"
"#
        )
    }
    #[test]
    fn rejects_unsatisfiable_constraint_types() {
        for field_type in ["string", "bool", "list"] {
            assert!(
                parse_automaton(&spec(field_type, "\"value\"", "param_greater_than_field"))
                    .is_err()
            );
        }
        assert!(
            parse_automaton(&spec(
                "counter",
                r#"{name="value",type="string"}"#,
                "param_equals_field"
            ))
            .is_err()
        );
        assert!(parse_automaton(&spec("counter", "\"value\"", "param_greater_than_field")).is_ok());
        assert!(
            parse_automaton(&spec(
                "counter",
                r#"{name="value",type="integer"}"#,
                "param_equals_field"
            ))
            .is_ok()
        );
    }
    #[test]
    fn rejects_invalid_strict_counter_default() {
        assert!(
            parse_automaton(
                &spec("counter", "\"value\"", "param_equals_field")
                    .replace("initial = \"0\"", "initial = \"-1\"")
            )
            .is_err()
        );
    }

    #[test]
    fn rejects_duplicate_action_contracts_before_last_rule_can_replace_first() {
        let source = spec("counter", "\"value\"", "param_equals_field");
        let duplicate = format!(
            "{source}\n[[action]]\nname = \"Observe\"\nfrom = [\"Ready\"]\nparams = [\"sequence\"]\n"
        );
        assert!(parse_automaton(&duplicate).is_err());
        assert!(
            parse_automaton(&duplicate.replace(
                "strict_action_params = true",
                "strict_action_params = false"
            ))
            .is_err()
        );
    }

    #[test]
    fn rejects_malformed_strict_integer_defaults() {
        for kind in ["int", "integer"] {
            for value in ["not-an-integer", "1.5", "9223372036854775808"] {
                let source = spec(kind, "\"value\"", "param_equals_field")
                    .replace("initial = \"0\"", &format!("initial = \"{value}\""));
                assert!(
                    parse_automaton(&source).is_err(),
                    "accepted {kind} default {value}"
                );
            }
            assert!(
                parse_automaton(
                    &spec(kind, "\"value\"", "param_equals_field")
                        .replace("initial = \"0\"", "initial = \"-3\"")
                )
                .is_ok()
            );
        }
    }

    #[test]
    fn rejects_constraint_references_without_a_runtime_value() {
        for name in [
            "has_spec",
            "HasSpec",
            "ctx_owner_status",
            "Status",
            "status",
        ] {
            let source = spec("counter", "\"value\"", "param_equals_field")
                .replace("field = \"sequence\"", &format!("field = \"{name}\""));
            assert!(
                parse_automaton(&source).is_err(),
                "accepted unresolved field {name}"
            );
        }
        for name in ["Id", "id"] {
            let source = spec("counter", "\"value\"", "param_equals_field")
                .replace("field = \"sequence\"", &format!("field = \"{name}\""));
            assert!(parse_automaton(&source).is_ok());
        }
    }
}
