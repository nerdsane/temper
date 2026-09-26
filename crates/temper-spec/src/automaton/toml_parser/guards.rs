use super::AutomatonParseError;
use super::values::{scalar_string, unsigned};
use crate::automaton::Guard;
use toml::{Table, Value};

/// Parse an action `guard` value: one string clause, or an array whose
/// entries are string clauses or `{ type = ... }` tables.
pub(super) fn parse_guard_value(
    value: &Value,
    guards: &mut Vec<Guard>,
) -> Result<(), AutomatonParseError> {
    match value {
        Value::String(clause) => guards.push(parse_guard_clause(clause)?),
        Value::Array(entries) => {
            for entry in entries {
                match entry {
                    Value::String(clause) => guards.push(parse_guard_clause(clause)?),
                    Value::Table(fields) => guards.push(parse_guard_fields(fields)?),
                    other => return Err(invalid_guard_value(other)),
                }
            }
        }
        Value::Table(fields) => guards.push(parse_guard_fields(fields)?),
        other => return Err(invalid_guard_value(other)),
    }
    Ok(())
}

fn invalid_guard_value(value: &Value) -> AutomatonParseError {
    AutomatonParseError::Validation(format!(
        "invalid guard '{value}' (expected a string clause or a {{ type = ... }} table)"
    ))
}

pub(super) fn parse_guard_clause(value: &str) -> Result<Guard, AutomatonParseError> {
    let trimmed = value.trim();

    for &(operator, is_min_guard) in &[(">=", true), ("<=", false), (">", true), ("<", false)] {
        if let Some(pos) = trimmed.find(operator) {
            return parse_infix_guard(trimmed, operator, pos, is_min_guard);
        }
    }

    if let Some(rest) = trimmed.strip_prefix('!') {
        return parse_negated_guard(trimmed, rest);
    }

    parse_prefix_guard(trimmed)
}

fn parse_guard_fields(fields: &Table) -> Result<Guard, AutomatonParseError> {
    let text = |key: &str| fields.get(key).and_then(scalar_string).unwrap_or_default();
    let list = |key: &str| string_list_field(fields, key);
    let count = |key: &str| unsigned::<usize>(fields, key).unwrap_or(0);
    let guard_type = text("type");

    let guard = match guard_type.as_str() {
        "cross_entity_state" => Guard::CrossEntityState {
            entity_type: text("entity_type"),
            entity_id_source: text("entity_id_source"),
            required_status: list("required_status"),
            forbidden_status: list("forbidden_status"),
            required: text("required").eq_ignore_ascii_case("true"),
        },
        "state_in" => Guard::StateIn {
            values: list("values"),
        },
        "min_count" => Guard::MinCount {
            var: text("var"),
            min: count("min"),
        },
        "max_count" => Guard::MaxCount {
            var: text("var"),
            max: count("max"),
        },
        "is_true" => Guard::IsTrue { var: text("var") },
        "is_false" => Guard::IsFalse { var: text("var") },
        "list_contains" => Guard::ListContains {
            var: text("var"),
            value: text("value"),
        },
        "list_length_min" => Guard::ListLengthMin {
            var: text("var"),
            min: count("min"),
        },
        _ => {
            return Err(AutomatonParseError::Validation(format!(
                "unsupported guard type '{guard_type}'"
            )));
        }
    };

    Ok(guard)
}

/// List-valued guard field; a lone scalar is a one-element list.
fn string_list_field(fields: &Table, key: &str) -> Vec<String> {
    match fields.get(key) {
        Some(Value::Array(items)) => items.iter().filter_map(scalar_string).collect(),
        Some(other) => scalar_string(other).into_iter().collect(),
        None => Vec::new(),
    }
    .into_iter()
    .filter(|s| !s.is_empty())
    .collect()
}

fn parse_infix_guard(
    trimmed: &str,
    operator: &str,
    position: usize,
    is_min_guard: bool,
) -> Result<Guard, AutomatonParseError> {
    let var = trimmed[..position].trim();
    let raw = trimmed[position + operator.len()..].trim();
    if var.is_empty() || raw.is_empty() {
        return Err(AutomatonParseError::Validation(format!(
            "invalid guard '{trimmed}' (expected '<var> {operator} <n>')"
        )));
    }

    let number = raw.parse::<usize>().map_err(|_| {
        AutomatonParseError::Validation(format!(
            "invalid guard '{trimmed}' (right side must be an integer)"
        ))
    })?;

    if is_min_guard {
        let min = if operator == ">=" { number } else { number + 1 };
        return Ok(Guard::MinCount {
            var: var.to_string(),
            min,
        });
    }

    let max = if operator == "<=" { number + 1 } else { number };
    Ok(Guard::MaxCount {
        var: var.to_string(),
        max,
    })
}

fn parse_negated_guard(trimmed: &str, rest: &str) -> Result<Guard, AutomatonParseError> {
    let var = rest.trim();
    if var.is_empty() || var.contains(' ') {
        return Err(AutomatonParseError::Validation(format!(
            "invalid guard '{trimmed}' (expected '!<var>')"
        )));
    }

    Ok(Guard::IsFalse {
        var: var.to_string(),
    })
}

fn parse_prefix_guard(trimmed: &str) -> Result<Guard, AutomatonParseError> {
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.is_empty() {
        return Err(AutomatonParseError::Validation(
            "empty guard clause".to_string(),
        ));
    }

    match parts[0] {
        "min" => Ok(Guard::MinCount {
            var: parts
                .get(1)
                .ok_or_else(|| invalid_guard(trimmed, "expected 'min <var> <n>'"))?
                .to_string(),
            min: parse_usize_arg(trimmed, parts.get(2), "min must be an integer")?,
        }),
        "max" => Ok(Guard::MaxCount {
            var: parts
                .get(1)
                .ok_or_else(|| invalid_guard(trimmed, "expected 'max <var> <n>'"))?
                .to_string(),
            max: parse_usize_arg(trimmed, parts.get(2), "max must be an integer")?,
        }),
        "is_true" => parse_boolean_guard(trimmed, &parts, true),
        "is_false" => parse_boolean_guard(trimmed, &parts, false),
        "list_contains" => {
            if parts.len() < 3 {
                return Err(invalid_guard(
                    trimmed,
                    "expected 'list_contains <var> <value>'",
                ));
            }
            Ok(Guard::ListContains {
                var: parts[1].to_string(),
                value: parts[2..].join(" "),
            })
        }
        "list_length_min" => Ok(Guard::ListLengthMin {
            var: parts
                .get(1)
                .ok_or_else(|| invalid_guard(trimmed, "expected 'list_length_min <var> <n>'"))?
                .to_string(),
            min: parse_usize_arg(trimmed, parts.get(2), "min must be an integer")?,
        }),
        _ if parts.len() == 1 && parts[0].chars().all(|c| c.is_alphanumeric() || c == '_') => {
            Ok(Guard::IsTrue {
                var: parts[0].to_string(),
            })
        }
        _ => Err(AutomatonParseError::Validation(format!(
            "unsupported guard syntax '{trimmed}'"
        ))),
    }
}

fn parse_boolean_guard(
    trimmed: &str,
    parts: &[&str],
    expected_true: bool,
) -> Result<Guard, AutomatonParseError> {
    if parts.len() != 2 {
        let expected = if expected_true {
            "expected 'is_true <var>'"
        } else {
            "expected 'is_false <var>'"
        };
        return Err(invalid_guard(trimmed, expected));
    }

    Ok(if expected_true {
        Guard::IsTrue {
            var: parts[1].to_string(),
        }
    } else {
        Guard::IsFalse {
            var: parts[1].to_string(),
        }
    })
}

fn parse_usize_arg(
    trimmed: &str,
    value: Option<&&str>,
    message: &str,
) -> Result<usize, AutomatonParseError> {
    let Some(value) = value else {
        return Err(invalid_guard(trimmed, "expected '<var> <n>'"));
    };

    value.parse().map_err(|_| {
        AutomatonParseError::Validation(format!("invalid guard '{trimmed}' ({message})"))
    })
}

fn invalid_guard(trimmed: &str, message: &str) -> AutomatonParseError {
    AutomatonParseError::Validation(format!("invalid guard '{trimmed}' ({message})"))
}
