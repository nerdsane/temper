//! Narrow deserialization compatibility for historical IOA syntax.

use serde::Deserialize;
use serde::de::{self, Deserializer};

use super::super::types::{CompositeCedarGate, Effect, Guard};

pub(in crate::automaton) fn deserialize_guards<'de, D>(
    deserializer: D,
) -> Result<Vec<Guard>, D::Error>
where
    D: Deserializer<'de>,
{
    match toml::Value::deserialize(deserializer)? {
        toml::Value::String(source) => parse_legacy_guard(&source)
            .map(|guard| vec![guard])
            .map_err(de::Error::custom),
        toml::Value::Array(guards) => guards
            .into_iter()
            .map(|entry| match entry {
                toml::Value::String(source) => parse_legacy_guard(&source),
                value @ toml::Value::Table(_) => {
                    Guard::deserialize(value).map_err(|error| error.to_string())
                }
                value => Err(format!(
                    "guard entries must be strings or tables, got {value}"
                )),
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(de::Error::custom),
        value => Err(de::Error::custom(format!(
            "action.guard must be a string or array, got {value}"
        ))),
    }
}

pub(in crate::automaton) fn deserialize_effects<'de, D>(
    deserializer: D,
) -> Result<Vec<Effect>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = toml::Value::deserialize(deserializer)?;
    deserialize_effect_value(value).map_err(de::Error::custom)
}

fn deserialize_effect_value(value: toml::Value) -> Result<Vec<Effect>, String> {
    match value {
        toml::Value::String(source) if source.trim_start().starts_with('[') => {
            // Some historical specs wrapped a structured effect array in a
            // TOML string. Decode only that field-local legacy payload; the
            // source document itself is still parsed once as one Automaton.
            let document = format!("effect = {source}");
            let mut wrapper = document
                .parse::<toml::Table>()
                .map_err(|error| format!("invalid legacy structured effect: {error}"))?;
            let value = wrapper
                .remove("effect")
                .ok_or_else(|| "legacy structured effect must declare an array".to_string())?;
            if let Some(unexpected) = wrapper.keys().next() {
                return Err(format!(
                    "legacy structured effect contains unexpected field `{unexpected}`"
                ));
            }
            deserialize_effect_value(value)
        }
        toml::Value::String(source) => parse_legacy_effect(&source).map(|effect| vec![effect]),
        toml::Value::Array(effects) => effects.into_iter().map(deserialize_effect_entry).collect(),
        value => Err(format!(
            "action.effect must be a string or array, got {value}"
        )),
    }
}

fn deserialize_effect_entry(value: toml::Value) -> Result<Effect, String> {
    match value {
        toml::Value::String(source) => parse_legacy_effect(&source),
        value @ toml::Value::Table(_) => {
            let effect_type = value
                .as_table()
                .and_then(|fields| fields.get("type"))
                .and_then(toml::Value::as_str)
                .ok_or_else(|| "structured effect must declare string field `type`".to_string())?;
            if !matches!(
                effect_type,
                "increment"
                    | "decrement"
                    | "set_counter_from_param"
                    | "set_bool"
                    | "emit"
                    | "emit_event"
                    | "list_append"
                    | "list_remove_at"
                    | "trigger"
                    | "schedule"
                    | "schedule_at"
                    | "spawn"
                    | "spawn_entity"
            ) {
                return Err(format!("unsupported effect type '{effect_type}'"));
            }
            Effect::deserialize(value).map_err(|error| error.to_string())
        }
        value => Err(format!(
            "effect entries must be strings or tables, got {value}"
        )),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum BoolSource {
    Bool(bool),
    String(String),
}

pub(in crate::automaton) fn deserialize_boolish<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    match BoolSource::deserialize(deserializer)? {
        BoolSource::Bool(value) => Ok(value),
        BoolSource::String(value) if value.eq_ignore_ascii_case("true") => Ok(true),
        BoolSource::String(value) if value.eq_ignore_ascii_case("false") => Ok(false),
        BoolSource::String(value) => Err(de::Error::custom(format!(
            "expected boolean or string \"true\"/\"false\", got {value:?}"
        ))),
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CopyFieldsSource {
    List(Vec<String>),
    CommaSeparated(String),
}

pub(in crate::automaton) fn deserialize_copy_fields<'de, D>(
    deserializer: D,
) -> Result<Option<Vec<String>>, D::Error>
where
    D: Deserializer<'de>,
{
    match CopyFieldsSource::deserialize(deserializer)? {
        CopyFieldsSource::List(fields) => Ok(Some(fields)),
        CopyFieldsSource::CommaSeparated(source) => {
            let fields = source
                .split(',')
                .map(str::trim)
                .filter(|field| !field.is_empty())
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>();
            Ok((!fields.is_empty()).then_some(fields))
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CedarGateSource {
    Single(CompositeCedarGate),
    Array(Vec<CompositeCedarGate>),
}

pub(in crate::automaton) fn deserialize_cedar_gate<'de, D>(
    deserializer: D,
) -> Result<Option<CompositeCedarGate>, D::Error>
where
    D: Deserializer<'de>,
{
    match CedarGateSource::deserialize(deserializer)? {
        CedarGateSource::Single(gate) => Ok(Some(gate)),
        CedarGateSource::Array(mut gates) if gates.len() == 1 => Ok(gates.pop()),
        CedarGateSource::Array(gates) => Err(de::Error::custom(format!(
            "action.cedar_gate must be declared exactly once, found {} declarations",
            gates.len()
        ))),
    }
}

fn parse_legacy_guard(source: &str) -> Result<Guard, String> {
    let source = source.trim();
    for &(operator, is_minimum) in &[(">=", true), ("<=", false), (">", true), ("<", false)] {
        if let Some(position) = source.find(operator) {
            return parse_infix_guard(source, operator, position, is_minimum);
        }
    }

    if let Some(rest) = source.strip_prefix('!') {
        let var = rest.trim();
        if var.is_empty() || var.contains(char::is_whitespace) {
            return Err(format!("invalid guard {source:?} (expected '!<var>')"));
        }
        return Ok(Guard::IsFalse {
            var: var.to_string(),
        });
    }

    parse_prefix_guard(source)
}

fn parse_infix_guard(
    source: &str,
    operator: &str,
    position: usize,
    is_minimum: bool,
) -> Result<Guard, String> {
    let var = source[..position].trim();
    let raw_number = source[position + operator.len()..].trim();
    if var.is_empty() || raw_number.is_empty() {
        return Err(format!(
            "invalid guard {source:?} (expected '<var> {operator} <n>')"
        ));
    }
    let number = raw_number
        .parse::<usize>()
        .map_err(|_| format!("invalid guard {source:?} (right side must be an integer)"))?;

    if is_minimum {
        let min = if operator == ">=" {
            number
        } else {
            number
                .checked_add(1)
                .ok_or_else(|| format!("invalid guard {source:?} (integer overflow)"))?
        };
        Ok(Guard::MinCount {
            var: var.to_string(),
            min,
        })
    } else {
        let max = if operator == "<" {
            number
        } else {
            number
                .checked_add(1)
                .ok_or_else(|| format!("invalid guard {source:?} (integer overflow)"))?
        };
        Ok(Guard::MaxCount {
            var: var.to_string(),
            max,
        })
    }
}

fn parse_prefix_guard(source: &str) -> Result<Guard, String> {
    let parts = source.split_whitespace().collect::<Vec<_>>();
    match parts.as_slice() {
        ["min", var, value] => Ok(Guard::MinCount {
            var: (*var).to_string(),
            min: parse_guard_number(source, value)?,
        }),
        ["max", var, value] => Ok(Guard::MaxCount {
            var: (*var).to_string(),
            max: parse_guard_number(source, value)?,
        }),
        ["is_true", var] => Ok(Guard::IsTrue {
            var: (*var).to_string(),
        }),
        ["is_false", var] => Ok(Guard::IsFalse {
            var: (*var).to_string(),
        }),
        ["list_length_min", var, value] => Ok(Guard::ListLengthMin {
            var: (*var).to_string(),
            min: parse_guard_number(source, value)?,
        }),
        ["list_contains", var, values @ ..] if !values.is_empty() => Ok(Guard::ListContains {
            var: (*var).to_string(),
            value: values.join(" "),
        }),
        [var]
            if var
                .chars()
                .all(|character| character.is_alphanumeric() || character == '_') =>
        {
            Ok(Guard::IsTrue {
                var: (*var).to_string(),
            })
        }
        _ => Err(format!("unsupported guard syntax {source:?}")),
    }
}

fn parse_guard_number(source: &str, value: &str) -> Result<usize, String> {
    value
        .parse()
        .map_err(|_| format!("invalid guard {source:?} (expected an unsigned integer)"))
}

fn parse_legacy_effect(source: &str) -> Result<Effect, String> {
    let source = source.trim();
    if let Some((var, amount)) = parse_counter_effect(source, "increment ") {
        return Ok(Effect::Increment { var, amount });
    }
    if let Some((var, amount)) = parse_counter_effect(source, "decrement ") {
        return Ok(Effect::Decrement { var, amount });
    }
    if let Some(rest) = source.strip_prefix("set ") {
        let parts = rest.split_whitespace().collect::<Vec<_>>();
        return match parts.as_slice() {
            [var, "true"] => Ok(Effect::SetBool {
                var: (*var).to_string(),
                value: true,
            }),
            [var, "false"] => Ok(Effect::SetBool {
                var: (*var).to_string(),
                value: false,
            }),
            _ => Err(format!(
                "invalid effect {source:?} (expected 'set <var> true|false')"
            )),
        };
    }
    if let Some(event) = parse_prefixed_identifier(source, "emit ") {
        return Ok(Effect::Emit { event });
    }
    if let Some(rest) = source.strip_prefix("schedule_at ") {
        let parts = rest.split_whitespace().collect::<Vec<_>>();
        return match parts.as_slice() {
            [field, action] => Ok(Effect::ScheduleAt {
                action: (*action).to_string(),
                field: (*field).to_string(),
            }),
            _ => Err(format!(
                "invalid effect {source:?} (expected 'schedule_at <field> <action>')"
            )),
        };
    }
    if let Some(name) = parse_prefixed_identifier(source, "trigger ") {
        return Ok(Effect::Trigger { name });
    }

    Err(format!("unsupported effect syntax {source:?}"))
}

fn parse_counter_effect(source: &str, prefix: &str) -> Option<(String, Option<String>)> {
    let rest = source.strip_prefix(prefix)?.trim();
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

fn parse_prefixed_identifier(source: &str, prefix: &str) -> Option<String> {
    source
        .strip_prefix(prefix)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}
