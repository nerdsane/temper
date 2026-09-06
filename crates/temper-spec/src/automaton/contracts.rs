//! Validate parameter constraints before a specification is installed.
use super::{ActionConstraint, ActionParam, Automaton, parser::AutomatonParseError};

fn category(kind: &str) -> &str {
    match kind {
        "counter" | "int" | "integer" => "integer",
        "status" => "string",
        other => other,
    }
}

pub(super) fn validate(automaton: &Automaton) -> Result<(), AutomatonParseError> {
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
                    } else if super::types::is_server_derived_field_name(name) {
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
}
