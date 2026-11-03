//! TOML reader for I/O Automaton specifications.
//!
//! The whole document is parsed once by the `toml` crate and every section
//! deserializes into its type: values are read in their TOML type, and an
//! unknown section or key is an error. Entries are read one at a time so an
//! error names the entry it is about (`action 'Submit': ...`). Predicates are
//! read in [`predicates`], effects in [`effects`]; both point specs still in
//! an old syntax at `temper migrate-predicates`.

mod effects;
mod predicates;

use super::parser::AutomatonParseError;
use super::types::*;
use effects::parse_effects;
use serde::de::DeserializeOwned;
use toml::{Table, Value};

/// Appended to errors for specs that `temper migrate-predicates` converts.
pub(super) const MIGRATE_HINT: &str = "convert the spec with `temper migrate-predicates`";

/// The sections a spec may contain.
pub(super) const SECTIONS: [&str; 15] = [
    "automaton",
    "state",
    "action",
    "invariant",
    "entry_rule",
    "liveness",
    "webhook",
    "context_entity",
    "field_invariant",
    "state_timeout",
    "key",
    "vector",
    "admission",
    "group",
    "carry",
];

/// Parse TOML into an Automaton struct.
pub(super) fn parse_toml_to_automaton(input: &str) -> Result<Automaton, AutomatonParseError> {
    let doc: Table = input
        .parse()
        .map_err(|e: toml::de::Error| AutomatonParseError::Toml(e.to_string()))?;
    read(&doc).map_err(|error| {
        let message = error.to_string();
        if message.contains("migrate-predicates") || !super::legacy::uses_old_syntax(input) {
            return error;
        }
        match error {
            AutomatonParseError::Toml(m) => {
                AutomatonParseError::Toml(format!("{m}; {MIGRATE_HINT}"))
            }
            AutomatonParseError::Validation(m) => {
                AutomatonParseError::Validation(format!("{m}; {MIGRATE_HINT}"))
            }
        }
    })
}

fn read(doc: &Table) -> Result<Automaton, AutomatonParseError> {
    if doc.contains_key("integration") {
        return Err(AutomatonParseError::Validation(
            "[[integration]] is no longer supported: declare the call as an [[action.triggers]] block on the action that fires it (`temper migrate-predicates` converts it)".into(),
        ));
    }
    if let Some(unknown) = doc.keys().find(|key| !SECTIONS.contains(&key.as_str())) {
        return Err(AutomatonParseError::Toml(format!(
            "unknown section '{unknown}' (a spec has {})",
            SECTIONS.join(", ")
        )));
    }
    let meta = match doc.get("automaton") {
        Some(Value::Table(meta)) => meta,
        Some(_) => {
            return Err(AutomatonParseError::Toml(
                "'automaton' must be written as [automaton]".into(),
            ));
        }
        None => return Err(AutomatonParseError::Toml("missing [automaton]".into())),
    };
    retired("[automaton]", meta, AUTOMATON_RETIRED)?;

    let automaton = Automaton {
        automaton: typed(Value::Table(meta.clone()), "automaton")?,
        state: entries(doc, "state", "name", typed_entry)?,
        actions: entries(doc, "action", "name", parse_action)?,
        invariants: predicates::invariants(doc)?,
        entry_rules: entries(doc, "entry_rule", "name", typed_entry)?,
        liveness: entries(doc, "liveness", "name", typed_entry)?,
        integrations: Vec::new(),
        webhooks: entries(doc, "webhook", "name", |scope, table| {
            retired(scope, table, WEBHOOK_RETIRED)?;
            typed_entry(scope, table)
        })?,
        context_entities: entries(doc, "context_entity", "name", typed_entry)?,
        field_invariants: predicates::field_invariants(doc)?,
        state_timeouts: entries(doc, "state_timeout", "state", |scope, table| {
            retired(scope, table, TIMEOUT_RETIRED)?;
            typed_entry(scope, table)
        })?,
        keys: entries(doc, "key", "name", typed_entry)?,
        vectors: entries(doc, "vector", "name", typed_entry)?,
        carries: entries(doc, "carry", "from", typed_entry)?,
        admission: doc
            .get("admission")
            .map(|value| typed(value.clone(), "admission"))
            .transpose()?,
        groups: entries(doc, "group", "name", typed_entry)?,
    };

    debug_assert!(automaton.actions.iter().all(|a| !a.name.is_empty()));
    debug_assert!(automaton.state.iter().all(|s| !s.name.is_empty()));
    Ok(automaton)
}

/// Keys that were renamed or retired, with what to write instead.
pub(super) type Retired = [(&'static str, &'static str)];

const AUTOMATON_RETIRED: &Retired = &[(
    "timeouts",
    "declare a [[state_timeout]] with after_seconds and on_timeout for each state",
)];
const TIMEOUT_RETIRED: &Retired = &[(
    "params",
    "write `args`, with string literals in single quotes: args = { error_message = \"'...'\" }",
)];
const WEBHOOK_RETIRED: &Retired = &[
    ("entity_lookup", "write entity_id = \"query.<name>\""),
    ("entity_param", "write entity_id = \"query.<name>\""),
];
/// Retired `[[action.triggers]]` keys.
pub(super) const TRIGGER_RETIRED: &Retired = &[
    (
        "to_state",
        "write the status into the guard, such as guard = \"status == 'Ready'\"",
    ),
    (
        "params",
        "write the values in `args`, string literals in single quotes: args = { source = \"'doc'\" }",
    ),
    (
        "params_from",
        "write the field names in `args`: args = { doc_id = \"Id\" }",
    ),
    ("adapter_type", "write `adapter`"),
];
/// Retired `resolve_target` keys.
pub(super) const RESOLVER_RETIRED: &Retired =
    &[("type", "write `kind`"), ("field", "write `id_field`")];

/// Fail on the first retired key in `table`.
pub(super) fn retired(
    scope: &str,
    table: &Table,
    keys: &Retired,
) -> Result<(), AutomatonParseError> {
    match keys.iter().find(|(key, _)| table.contains_key(*key)) {
        Some((key, instead)) => Err(AutomatonParseError::Validation(format!(
            "{scope}: `{key}` is no longer a key; {instead}"
        ))),
        None => Ok(()),
    }
}

/// Deserialize `value`, naming `scope` and the offending key on error.
pub(super) fn typed<T: DeserializeOwned>(
    value: Value,
    scope: &str,
) -> Result<T, AutomatonParseError> {
    value.try_into().map_err(|e: toml::de::Error| {
        AutomatonParseError::Toml(format!("{scope}: {}", describe(&e)))
    })
}

/// Render a `toml` deserialization error as `key: problem`.
fn describe(error: &toml::de::Error) -> String {
    let message = error.to_string();
    let message = message.trim();
    match message.rsplit_once("\nin `") {
        Some((problem, key)) => format!("{}: {}", key.trim_end_matches('`'), problem.trim()),
        None => message.to_string(),
    }
}

fn typed_entry<T: DeserializeOwned>(scope: &str, table: &Table) -> Result<T, AutomatonParseError> {
    typed(Value::Table(table.clone()), scope)
}

/// Read every `[[key]]` entry with `parse`. Each entry is labelled by its
/// `label` key (which must be a non-empty string when it is `name`).
pub(super) fn entries<T>(
    doc: &Table,
    key: &str,
    label: &str,
    parse: fn(&str, &Table) -> Result<T, AutomatonParseError>,
) -> Result<Vec<T>, AutomatonParseError> {
    let Some(value) = doc.get(key) else {
        return Ok(Vec::new());
    };
    let not_array = || AutomatonParseError::Toml(format!("'{key}' must be written as [[{key}]]"));
    let Value::Array(entries) = value else {
        return Err(not_array());
    };
    let mut out = Vec::with_capacity(entries.len());
    for (index, entry) in entries.iter().enumerate() {
        let Value::Table(table) = entry else {
            return Err(not_array());
        };
        let scope = match table.get(label) {
            Some(Value::String(name)) if !name.is_empty() => format!("{key} '{name}'"),
            _ if label == "name" => {
                return Err(AutomatonParseError::Toml(format!(
                    "{key} #{}: every [[{key}]] needs a non-empty `name`",
                    index + 1
                )));
            }
            _ => format!("{key} #{}", index + 1),
        };
        out.push(parse(&scope, table)?);
    }
    Ok(out)
}

/// An `[[action]]`. `guard`, `effect` and `triggers` are read by their slot
/// readers (which recognise the old syntaxes); the rest deserializes.
fn parse_action(scope: &str, table: &Table) -> Result<Action, AutomatonParseError> {
    let name = table
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let mut rest = table.clone();
    let guard = rest
        .remove("guard")
        .map(|value| predicates::action_guard(name, &value))
        .transpose()?;
    let effect = rest
        .remove("effect")
        .map(|value| parse_effects(name, &value))
        .transpose()?;
    let triggers = rest
        .remove("triggers")
        .map(|value| predicates::triggers(scope, &value))
        .transpose()?;
    let writes_record_parent_event = rest.contains_key("record_parent_event");

    let mut action: Action = typed(Value::Table(rest), scope)?;
    action.guard = guard.unwrap_or_else(crate::predicate::Expr::always);
    action.effect = effect.unwrap_or_default();
    action.triggers = triggers.unwrap_or_default();

    if action.kind != ActionKind::Composite {
        let composite_only = [
            ("record_parent_event", writes_record_parent_event),
            ("cedar_gate", action.cedar_gate.is_some()),
            ("sub_writes", !action.sub_writes.is_empty()),
        ];
        if let Some((key, _)) = composite_only.iter().find(|(_, written)| *written) {
            return Err(AutomatonParseError::Validation(format!(
                "{scope}: {key} is only allowed on kind = \"composite\""
            )));
        }
    }
    Ok(action)
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
