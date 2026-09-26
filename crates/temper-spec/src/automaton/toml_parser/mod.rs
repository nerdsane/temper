//! TOML reader for I/O Automaton specifications.
//!
//! The whole document is parsed once by the `toml` crate. Sections with a
//! direct serde shape (`[[field_invariant]]`, `[[state_timeout]]`, `[[key]]`,
//! `[[vector]]`, `[[webhook]]`, `[[context_entity]]`, `[admission]` and the
//! nested `[[action.*]]` tables) deserialize straight into their types. The
//! core sections are read field by field so they keep the lenient value
//! rules in [`values`] and the string forms of guards and effects.

mod effects;
mod guards;
mod values;

use super::parser::AutomatonParseError;
use super::types::*;
use effects::parse_effect_value;
use guards::parse_guard_value;
use serde::de::DeserializeOwned;
use toml::{Table, Value};
use values::{any_string, bool_value, string, string_list, unsigned};

/// Parse one legacy guard clause such as `is_true ready` (used when lowering
/// asserts written in guard syntax).
pub(crate) fn legacy_guard_clause(clause: &str) -> Option<Guard> {
    guards::parse_guard_clause(clause).ok()
}

/// Parse TOML into an Automaton struct.
pub(super) fn parse_toml_to_automaton(input: &str) -> Result<Automaton, AutomatonParseError> {
    let doc: Table = input
        .parse()
        .map_err(|e: toml::de::Error| AutomatonParseError::Toml(e.to_string()))?;

    let automaton = Automaton {
        automaton: parse_meta(&doc)?,
        state: named_items(&doc, "state", parse_state_var)?,
        actions: named_items(&doc, "action", parse_action)?,
        invariants: named_items(&doc, "invariant", parse_invariant)?,
        liveness: named_items(&doc, "liveness", parse_liveness)?,
        integrations: named_items(&doc, "integration", parse_integration)?,
        webhooks: section(&doc, "webhook")?,
        context_entities: section(&doc, "context_entity")?,
        field_invariants: section(&doc, "field_invariant")?,
        state_timeouts: section(&doc, "state_timeout")?,
        keys: section(&doc, "key")?,
        vectors: section(&doc, "vector")?,
        admission: section(&doc, "admission")?,
    };

    debug_assert!(automaton.actions.iter().all(|a| !a.name.is_empty()));
    Ok(automaton)
}

/// Deserialize a top-level key straight into its type; absent means default.
fn section<T: DeserializeOwned + Default>(
    doc: &Table,
    key: &str,
) -> Result<T, AutomatonParseError> {
    match doc.get(key) {
        Some(value) => deserialize(value, key),
        None => Ok(T::default()),
    }
}

fn deserialize<T: DeserializeOwned>(value: &Value, scope: &str) -> Result<T, AutomatonParseError> {
    value
        .clone()
        .try_into()
        .map_err(|e: toml::de::Error| AutomatonParseError::Toml(format!("{scope}: {e}")))
}

/// Read an array of tables with `parse`, dropping entries without a `name`.
fn named_items<T>(
    doc: &Table,
    key: &str,
    parse: fn(&Table) -> Result<(String, T), AutomatonParseError>,
) -> Result<Vec<T>, AutomatonParseError> {
    let Some(value) = doc.get(key) else {
        return Ok(Vec::new());
    };
    let not_array = || AutomatonParseError::Toml(format!("'{key}' must be written as [[{key}]]"));
    let Value::Array(entries) = value else {
        return Err(not_array());
    };
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let Value::Table(table) = entry else {
            return Err(not_array());
        };
        let (name, item) = parse(table)?;
        if !name.is_empty() {
            out.push(item);
        }
    }
    Ok(out)
}

fn parse_meta(doc: &Table) -> Result<AutomatonMeta, AutomatonParseError> {
    const S: &str = "automaton";
    let empty = Table::new();
    let table = match doc.get(S) {
        Some(Value::Table(table)) => table,
        Some(_) => {
            return Err(AutomatonParseError::Toml(
                "'automaton' must be written as [automaton]".into(),
            ));
        }
        None => &empty,
    };
    let strict_action_params = match table.get("strict_action_params") {
        None => false,
        Some(_) => bool_value(table, "strict_action_params").ok_or_else(|| {
            AutomatonParseError::Validation("strict_action_params must be true or false".into())
        })?,
    };
    Ok(AutomatonMeta {
        name: string(table, S, "name")?.unwrap_or_default(),
        states: string_list(table, S, "states")?,
        initial: string(table, S, "initial")?.unwrap_or_default(),
        allow_indefinite_states: string_list(table, S, "allow_indefinite_states")?,
        strict_action_params,
    })
}

fn parse_state_var(table: &Table) -> Result<(String, StateVar), AutomatonParseError> {
    const S: &str = "state";
    let name = string(table, S, "name")?.unwrap_or_default();
    let var = StateVar {
        name: name.clone(),
        var_type: string(table, S, "type")?.unwrap_or_else(|| "string".into()),
        initial: string(table, S, "initial")?.unwrap_or_default(),
        overflow_inline_max_bytes: unsigned(table, "overflow_inline_max_bytes"),
        overflow_ttl_seconds: unsigned(table, "overflow_ttl_seconds"),
        query_indexed: bool_value(table, "query_indexed"),
    };
    Ok((name, var))
}

fn parse_action(table: &Table) -> Result<(String, Action), AutomatonParseError> {
    const S: &str = "action";
    let name = string(table, S, "name")?.unwrap_or_default();
    let mut guard = Vec::new();
    if let Some(value) = table.get("guard") {
        parse_guard_value(value, &mut guard)?;
    }
    let mut effect = Vec::new();
    if let Some(value) = table.get("effect") {
        parse_effect_value(value, &mut effect)?;
    }
    let cedar_gate = match table.get("cedar_gate") {
        // `[[action.cedar_gate]]` is an array; the first gate applies.
        Some(Value::Array(gates)) => gates.first(),
        other => other,
    };
    let action = Action {
        name: name.clone(),
        kind: string(table, S, "kind")?.unwrap_or_else(|| "internal".into()),
        from: string_list(table, S, "from")?,
        to: string(table, S, "to")?,
        guard,
        effect,
        params: parse_action_params(table)?,
        constraints: nested(table, "constraints")?,
        hint: string(table, S, "hint")?,
        record_parent_event: bool_value(table, "record_parent_event").unwrap_or(true),
        triggers: nested(table, "triggers")?,
        cedar_gate: cedar_gate
            .map(|gate| deserialize(gate, "action metadata"))
            .transpose()?,
        sub_writes: nested(table, "sub_writes")?,
    };
    Ok((name, action))
}

/// Deserialize a nested `[[action.<key>]]` array straight into its type.
fn nested<T: DeserializeOwned>(table: &Table, key: &str) -> Result<Vec<T>, AutomatonParseError> {
    table
        .get(key)
        .map(|value| deserialize(value, "action metadata"))
        .transpose()
        .map(Option::unwrap_or_default)
}

/// Action `params`: names (`["a", "b"]`), typed tables, or a single name.
fn parse_action_params(table: &Table) -> Result<Vec<ActionParam>, AutomatonParseError> {
    let entries = match table.get("params") {
        None => return Ok(Vec::new()),
        Some(Value::Array(entries)) => entries.clone(),
        Some(name @ Value::String(_)) => vec![name.clone()],
        Some(_) => {
            return Err(AutomatonParseError::Validation(
                "invalid typed action parameters: 'params' must be a list".into(),
            ));
        }
    };
    let params: Vec<ActionParam> =
        Value::Array(entries)
            .try_into()
            .map_err(|e: toml::de::Error| {
                AutomatonParseError::Validation(format!("invalid typed action parameters: {e}"))
            })?;
    Ok(params
        .into_iter()
        .filter(|param| !param.name().is_empty())
        .collect())
}

fn parse_invariant(table: &Table) -> Result<(String, Invariant), AutomatonParseError> {
    const S: &str = "invariant";
    let name = string(table, S, "name")?.unwrap_or_default();
    let invariant = Invariant {
        name: name.clone(),
        when: string_list(table, S, "when")?,
        assert: string(table, S, "assert")?.unwrap_or_default(),
    };
    Ok((name, invariant))
}

fn parse_liveness(table: &Table) -> Result<(String, Liveness), AutomatonParseError> {
    const S: &str = "liveness";
    let name = string(table, S, "name")?.unwrap_or_default();
    let liveness = Liveness {
        name: name.clone(),
        from: string_list(table, S, "from")?,
        reaches: string_list(table, S, "reaches")?,
        has_actions: table
            .get("has_actions")
            .map(|_| bool_value(table, "has_actions") == Some(true)),
    };
    Ok((name, liveness))
}

/// `[[integration]]`: known keys map to fields; every other key, and every
/// entry of an `[integration.config]` table, is config.
fn parse_integration(table: &Table) -> Result<(String, Integration), AutomatonParseError> {
    let mut integration = Integration {
        name: String::new(),
        trigger: String::new(),
        integration_type: "webhook".into(),
        module: None,
        on_success: None,
        on_failure: None,
        llm: false,
        config: std::collections::BTreeMap::new(),
    };
    for (key, value) in table {
        match key.as_str() {
            "name" => integration.name = any_string(value),
            "trigger" => integration.trigger = any_string(value),
            "type" => integration.integration_type = any_string(value),
            "module" => integration.module = Some(any_string(value)),
            "on_success" => integration.on_success = Some(any_string(value)),
            "on_failure" => integration.on_failure = Some(any_string(value)),
            "llm" => integration.llm = bool_value(table, "llm") == Some(true),
            // `[integration.config]` entries are config keys, like inline ones.
            "config" if value.is_table() => {
                for (config_key, config_value) in value.as_table().into_iter().flatten() {
                    integration
                        .config
                        .insert(config_key.clone(), any_string(config_value));
                }
            }
            _ => {
                integration.config.insert(key.clone(), any_string(value));
            }
        }
    }
    Ok((integration.name.clone(), integration))
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
