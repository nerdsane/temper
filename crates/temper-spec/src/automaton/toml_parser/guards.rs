use super::AutomatonParseError;
use super::inline::{parse_inline_fields, parse_string_array, split_inline_tables};
use crate::automaton::Guard;

pub(super) fn parse_guard_value(
    value: &str,
    guards: &mut Vec<Guard>,
) -> Result<(), AutomatonParseError> {
    let trimmed = value.trim();

    if trimmed.starts_with('[') && trimmed.contains('{') {
        return parse_guard_array(trimmed, guards);
    }

    guards.push(parse_guard_clause(trimmed)?);
    Ok(())
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

fn parse_guard_array(value: &str, guards: &mut Vec<Guard>) -> Result<(), AutomatonParseError> {
    let trimmed = value.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return Ok(());
    }

    let inner = &trimmed[1..trimmed.len() - 1];
    for entry in split_inline_tables(inner) {
        let entry = entry.trim().trim_matches('{').trim_matches('}').trim();
        guards.push(parse_guard_fields(&parse_inline_fields(entry))?);
    }

    Ok(())
}

fn parse_guard_fields(
    fields: &std::collections::BTreeMap<String, String>,
) -> Result<Guard, AutomatonParseError> {
    let guard_type = fields.get("type").map(|s| s.as_str()).unwrap_or("");

    let guard = match guard_type {
        "cross_entity_state" => Guard::CrossEntityState {
            entity_type: fields.get("entity_type").cloned().unwrap_or_default(),
            entity_id_source: fields.get("entity_id_source").cloned().unwrap_or_default(),
            required_status: fields
                .get("required_status")
                .map(|s| parse_string_array(s))
                .unwrap_or_default(),
        },
        "state_in" => Guard::StateIn {
            values: fields
                .get("values")
                .map(|s| parse_string_array(s))
                .unwrap_or_default(),
        },
        "min_count" => Guard::MinCount {
            var: fields.get("var").cloned().unwrap_or_default(),
            min: fields.get("min").and_then(|s| s.parse().ok()).unwrap_or(0),
        },
        "max_count" => Guard::MaxCount {
            var: fields.get("var").cloned().unwrap_or_default(),
            max: fields.get("max").and_then(|s| s.parse().ok()).unwrap_or(0),
        },
        "is_true" => Guard::IsTrue {
            var: fields.get("var").cloned().unwrap_or_default(),
        },
        "is_false" => Guard::IsFalse {
            var: fields.get("var").cloned().unwrap_or_default(),
        },
        "list_contains" => Guard::ListContains {
            var: fields.get("var").cloned().unwrap_or_default(),
            value: fields.get("value").cloned().unwrap_or_default(),
        },
        "list_length_min" => Guard::ListLengthMin {
            var: fields.get("var").cloned().unwrap_or_default(),
            min: fields.get("min").and_then(|s| s.parse().ok()).unwrap_or(0),
        },
        _ => {
            return Err(AutomatonParseError::Validation(format!(
                "unsupported guard type '{guard_type}'"
            )));
        }
    };

    Ok(guard)
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
