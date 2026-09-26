use super::AutomatonParseError;
use super::values::{scalar_string, unsigned};
use crate::automaton::Effect;
use toml::{Table, Value};

/// Parse an action `effect` value: one string effect, or an array whose
/// entries are string effects or `{ type = ... }` tables.
pub(super) fn parse_effect_value(
    value: &Value,
    effects: &mut Vec<Effect>,
) -> Result<(), AutomatonParseError> {
    match value {
        Value::String(text) => effects.push(parse_string_effect(text)?),
        Value::Array(entries) => {
            for entry in entries {
                match entry {
                    Value::String(text) => effects.push(parse_string_effect(text)?),
                    Value::Table(fields) => effects.extend(parse_effect_fields(fields)?),
                    other => return Err(invalid_effect_value(other)),
                }
            }
        }
        Value::Table(fields) => effects.extend(parse_effect_fields(fields)?),
        other => return Err(invalid_effect_value(other)),
    }
    Ok(())
}

fn invalid_effect_value(value: &Value) -> AutomatonParseError {
    AutomatonParseError::Validation(format!(
        "invalid effect '{value}' (expected a string effect or a {{ type = ... }} table)"
    ))
}

fn parse_string_effect(text: &str) -> Result<Effect, AutomatonParseError> {
    parse_legacy_effect(text.trim()).ok_or_else(|| {
        AutomatonParseError::Validation(format!("unsupported effect syntax '{}'", text.trim()))
    })
}

fn parse_effect_fields(fields: &Table) -> Result<Option<Effect>, AutomatonParseError> {
    let get = |key: &str| fields.get(key).and_then(scalar_string);
    let text = |key: &str| get(key).unwrap_or_default();
    let effect_type = text("type");

    let effect = match effect_type.as_str() {
        "schedule" => {
            let action = text("action");
            if action.is_empty() {
                None
            } else {
                Some(Effect::Schedule {
                    action,
                    delay_seconds: unsigned(fields, "delay_seconds").unwrap_or(0),
                })
            }
        }
        "schedule_at" => {
            let action = text("action");
            let field = text("field");
            if action.is_empty() || field.is_empty() {
                None
            } else {
                Some(Effect::ScheduleAt { action, field })
            }
        }
        "increment" => get("var").map(|var| Effect::Increment {
            var,
            amount: get("amount"),
        }),
        "decrement" => get("var").map(|var| Effect::Decrement {
            var,
            amount: get("amount"),
        }),
        "set_counter_from_param" => get("var").map(|var| {
            let param = get("param").unwrap_or_else(|| var.clone());
            Effect::SetCounterFromParam { var, param }
        }),
        "set_bool" => get("var").map(|var| Effect::SetBool {
            var,
            value: get("value").is_some_and(|s| s == "true"),
        }),
        "emit" | "emit_event" => get("event").map(|event| Effect::Emit { event }),
        "trigger" => get("name").map(|name| Effect::Trigger { name }),
        "list_append" => list_var(fields).map(|var| Effect::ListAppend { var }),
        "list_remove_at" => list_var(fields).map(|var| Effect::ListRemoveAt { var }),
        "spawn" | "spawn_entity" => {
            let entity_type = text("entity_type");
            if entity_type.is_empty() {
                None
            } else {
                Some(Effect::Spawn {
                    entity_type,
                    entity_id_source: text("entity_id_source"),
                    initial_action: get("initial_action"),
                    store_id_in: get("store_id_in"),
                    copy_fields: copy_fields(fields),
                })
            }
        }
        _ => {
            return Err(AutomatonParseError::Validation(format!(
                "unsupported effect type '{effect_type}'"
            )));
        }
    };

    Ok(effect)
}

/// `copy_fields` as a TOML array or a comma-separated string.
fn copy_fields(fields: &Table) -> Option<Vec<String>> {
    let names: Vec<String> = match fields.get("copy_fields")? {
        Value::Array(items) => items.iter().filter_map(scalar_string).collect(),
        other => scalar_string(other)?
            .split(',')
            .map(str::to_string)
            .collect(),
    };
    let names: Vec<String> = names
        .into_iter()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    if names.is_empty() { None } else { Some(names) }
}

fn parse_legacy_effect(value: &str) -> Option<Effect> {
    if let Some((var, amount)) = parse_counter_effect(value, "increment ") {
        return Some(Effect::Increment { var, amount });
    }

    if let Some((var, amount)) = parse_counter_effect(value, "decrement ") {
        return Some(Effect::Decrement { var, amount });
    }

    if let Some((var, bool_value)) = parse_bool_set(value) {
        return Some(Effect::SetBool {
            var,
            value: bool_value,
        });
    }

    if let Some(event) = parse_prefixed_identifier(value, "emit ") {
        return Some(Effect::Emit { event });
    }

    if let Some(rest) = value.strip_prefix("schedule_at ") {
        let parts: Vec<&str> = rest.splitn(2, ' ').collect();
        if parts.len() == 2 && !parts[0].is_empty() && !parts[1].is_empty() {
            return Some(Effect::ScheduleAt {
                field: parts[0].to_string(),
                action: parts[1].to_string(),
            });
        }
    }

    parse_prefixed_identifier(value, "trigger ").map(|name| Effect::Trigger { name })
}

fn parse_prefixed_identifier(value: &str, prefix: &str) -> Option<String> {
    value
        .strip_prefix(prefix)
        .map(str::trim)
        .filter(|candidate| !candidate.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_counter_effect(value: &str, prefix: &str) -> Option<(String, Option<String>)> {
    let rest = value.strip_prefix(prefix)?.trim();
    if rest.is_empty() {
        return None;
    }
    if let Some((var, amount)) = rest.split_once(" by ") {
        let var = var.trim();
        let amount = amount.trim();
        if var.is_empty() || amount.is_empty() {
            return None;
        }
        return Some((var.to_string(), Some(amount.to_string())));
    }
    Some((rest.to_string(), None))
}

fn parse_bool_set(value: &str) -> Option<(String, bool)> {
    let parts: Vec<&str> = value.splitn(3, ' ').collect();
    if parts.len() != 3 || parts[0] != "set" {
        return None;
    }

    Some((parts[1].to_string(), parts[2].trim() == "true"))
}

fn list_var(fields: &Table) -> Option<String> {
    fields
        .get("var")
        .or_else(|| fields.get("list"))
        .and_then(scalar_string)
}
