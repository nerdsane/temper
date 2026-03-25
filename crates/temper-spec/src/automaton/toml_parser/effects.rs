use super::AutomatonParseError;
use super::inline::{parse_inline_fields, split_inline_tables};
use crate::automaton::Effect;

pub(super) fn parse_effect_value(
    value: &str,
    effects: &mut Vec<Effect>,
) -> Result<(), AutomatonParseError> {
    let trimmed = value.trim();

    if trimmed.starts_with('[') && trimmed.contains('{') {
        return parse_effect_array(trimmed, effects);
    }

    if let Some(effect) = parse_legacy_effect(trimmed) {
        effects.push(effect);
    }

    Ok(())
}

fn parse_effect_array(value: &str, effects: &mut Vec<Effect>) -> Result<(), AutomatonParseError> {
    let trimmed = value.trim();
    if !trimmed.starts_with('[') || !trimmed.ends_with(']') {
        return Ok(());
    }

    let inner = &trimmed[1..trimmed.len() - 1];
    for entry in split_inline_tables(inner) {
        let entry = entry.trim().trim_matches('{').trim_matches('}').trim();
        let fields = parse_inline_fields(entry);

        if let Some(effect) = parse_effect_fields(&fields)? {
            effects.push(effect);
        }
    }

    Ok(())
}

fn parse_effect_fields(
    fields: &std::collections::BTreeMap<String, String>,
) -> Result<Option<Effect>, AutomatonParseError> {
    let effect_type = fields.get("type").map(|s| s.as_str()).unwrap_or("");

    let effect = match effect_type {
        "schedule" => {
            let action = fields.get("action").cloned().unwrap_or_default();
            if action.is_empty() {
                None
            } else {
                let delay_seconds = fields
                    .get("delay_seconds")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                Some(Effect::Schedule {
                    action,
                    delay_seconds,
                })
            }
        }
        "increment" => fields
            .get("var")
            .cloned()
            .map(|var| Effect::Increment { var }),
        "decrement" => fields
            .get("var")
            .cloned()
            .map(|var| Effect::Decrement { var }),
        "set_bool" => fields.get("var").cloned().map(|var| Effect::SetBool {
            var,
            value: fields.get("value").is_some_and(|s| s == "true"),
        }),
        "emit" | "emit_event" => fields
            .get("event")
            .cloned()
            .map(|event| Effect::Emit { event }),
        "trigger" => fields
            .get("name")
            .cloned()
            .map(|name| Effect::Trigger { name }),
        "list_append" => list_var(fields).map(|var| Effect::ListAppend { var }),
        "list_remove_at" => list_var(fields).map(|var| Effect::ListRemoveAt { var }),
        "spawn" | "spawn_entity" => {
            let entity_type = fields.get("entity_type").cloned().unwrap_or_default();
            if entity_type.is_empty() {
                None
            } else {
                Some(Effect::Spawn {
                    entity_type,
                    entity_id_source: fields.get("entity_id_source").cloned().unwrap_or_default(),
                    initial_action: fields.get("initial_action").cloned(),
                    store_id_in: fields.get("store_id_in").cloned(),
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

fn parse_legacy_effect(value: &str) -> Option<Effect> {
    if let Some(var) = parse_prefixed_identifier(value, "increment ") {
        return Some(Effect::Increment { var });
    }

    if let Some(var) = parse_prefixed_identifier(value, "decrement ") {
        return Some(Effect::Decrement { var });
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

    parse_prefixed_identifier(value, "trigger ").map(|name| Effect::Trigger { name })
}

fn parse_prefixed_identifier(value: &str, prefix: &str) -> Option<String> {
    value
        .strip_prefix(prefix)
        .map(str::trim)
        .filter(|candidate| !candidate.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_bool_set(value: &str) -> Option<(String, bool)> {
    let parts: Vec<&str> = value.splitn(3, ' ').collect();
    if parts.len() != 3 || parts[0] != "set" {
        return None;
    }

    Some((parts[1].to_string(), parts[2].trim() == "true"))
}

fn list_var(fields: &std::collections::BTreeMap<String, String>) -> Option<String> {
    fields
        .get("var")
        .cloned()
        .or_else(|| fields.get("list").cloned())
}
