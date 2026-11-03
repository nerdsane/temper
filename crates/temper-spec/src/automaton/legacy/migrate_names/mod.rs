//! The value spellings and key names that preceded strict typed reading
//! (ADR-0181), rewritten to the current ones. Where the old reader ignored or
//! defaulted a value, the rewrite writes what it meant and adds a note.

mod triggers;

use toml_edit::{Array, DocumentMut, Item, TableLike, Value, value};

use crate::automaton::toml_parser::SECTIONS;

/// Rewrite old value spellings and key names in `doc`. With `drop_unknown`,
/// keys and sections no reader reads are removed (noted).
pub(super) fn migrate_names(
    doc: &mut DocumentMut,
    notes: &mut Vec<String>,
    drop_unknown: bool,
) -> Result<(), String> {
    let mut pass = Pass {
        notes,
        drop_unknown,
    };
    let unknown: Vec<String> = doc
        .iter()
        .map(|(key, _)| key.to_string())
        .filter(|key| !SECTIONS.contains(&key.as_str()))
        .collect();
    if drop_unknown {
        for key in unknown {
            doc.remove(&key);
            pass.note(format!("dropped section '{key}': nothing reads it"));
        }
    }
    if let Some(meta) = doc.get_mut("automaton").and_then(Item::as_table_like_mut) {
        pass.automaton(meta)?;
    }
    pass.entries(doc, "state", Pass::state)?;
    pass.entries(doc, "action", Pass::action)?;
    pass.entries(doc, "liveness", Pass::liveness)?;
    pass.entries(doc, "webhook", Pass::webhook)?;
    pass.entries(doc, "state_timeout", Pass::state_timeout)?;
    for (section, keys) in [
        ("invariant", &["name", "assert"][..]),
        ("entry_rule", &["name", "place", "from", "assert"][..]),
        ("field_invariant", &["name", "assert", "message"][..]),
        ("context_entity", &["name", "entity_type", "id_field"][..]),
        ("key", &["name", "properties"][..]),
        ("group", &["name", "says", "states"][..]),
        (
            "vector",
            &["name", "property", "model_property", "dims", "metric"][..],
        ),
    ] {
        pass.entries(doc, section, |pass, scope, table| {
            pass.known(scope, table, keys);
            Ok(())
        })?;
    }
    if let Some(admission) = doc.get_mut("admission").and_then(Item::as_table_like_mut) {
        pass.known(
            "admission",
            admission,
            &[
                "max_concurrent_creates",
                "max_concurrent_actions",
                "queue_depth",
                "queue_timeout_seconds",
            ],
        );
    }
    Ok(())
}

struct Pass<'a> {
    notes: &'a mut Vec<String>,
    drop_unknown: bool,
}

impl Pass<'_> {
    fn note(&mut self, note: String) {
        self.notes.push(note);
    }

    /// Visit every `[[key]]` entry; entries without a name (which the old
    /// reader skipped) are removed.
    fn entries<F>(&mut self, doc: &mut DocumentMut, key: &str, mut visit: F) -> Result<(), String>
    where
        F: FnMut(&mut Self, &str, &mut dyn TableLike) -> Result<(), String>,
    {
        let label = if key == "state_timeout" {
            "state"
        } else {
            "name"
        };
        let Some(item) = doc.get_mut(key) else {
            return Ok(());
        };
        match item {
            Item::ArrayOfTables(tables) => {
                let mut unnamed = Vec::new();
                for (index, table) in tables.iter_mut().enumerate() {
                    stringify(table, label);
                    let name = table
                        .get(label)
                        .and_then(Item::as_str)
                        .unwrap_or_default()
                        .to_string();
                    if name.is_empty() && label == "name" {
                        unnamed.push(index);
                        continue;
                    }
                    visit(self, &format!("{key} '{name}'"), table)?;
                }
                for index in unnamed.into_iter().rev() {
                    tables.remove(index);
                    self.note(format!(
                        "dropped a [[{key}]] without a name (it was never read)"
                    ));
                }
            }
            Item::Value(Value::Array(values)) => {
                for entry in values.iter_mut() {
                    if let Some(table) = entry.as_inline_table_mut() {
                        let name = table
                            .get(label)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        visit(self, &format!("{key} '{name}'"), table)?;
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Drop keys outside `keys` (the old reader ignored them).
    fn known(&mut self, scope: &str, table: &mut dyn TableLike, keys: &[&str]) {
        if !self.drop_unknown {
            return;
        }
        let unknown: Vec<String> = table
            .iter()
            .map(|(key, _)| key.to_string())
            .filter(|key| !keys.contains(&key.as_str()))
            .collect();
        for key in unknown {
            table.remove(&key);
            self.note(format!("{scope}: dropped `{key}` (nothing reads it)"));
        }
    }

    fn automaton(&mut self, meta: &mut dyn TableLike) -> Result<(), String> {
        for key in ["name", "initial"] {
            stringify(meta, key);
        }
        for key in ["states", "terminal", "allow_indefinite_states"] {
            listify(meta, key);
        }
        if let Some(text) = string_value(meta, "strict_action_params") {
            let flag = match text.as_str() {
                "true" => true,
                "false" => false,
                other => {
                    return Err(format!(
                        "strict_action_params must be true or false, found \"{other}\""
                    ));
                }
            };
            meta.insert("strict_action_params", value(flag));
        }
        if meta.remove("timeouts").is_some() {
            self.note(
                "dropped [automaton.timeouts]: nothing read it; declare a [[state_timeout]] to time a state out".into(),
            );
        }
        self.known(
            "[automaton]",
            meta,
            &[
                "name",
                "states",
                "initial",
                "allow_indefinite_states",
                "strict_action_params",
                "terminal",
            ],
        );
        Ok(())
    }

    fn state(&mut self, scope: &str, table: &mut dyn TableLike) -> Result<(), String> {
        stringify(table, "type");
        let var_type = match table.get("type").and_then(Item::as_str).unwrap_or("string") {
            "set" | "list" => "list",
            "status" | "string" => "string",
            "integer" | "int" => "int",
            "counter" => "counter",
            "bool" => "bool",
            other => {
                return Err(format!(
                    "{scope}: type `{other}` has no current equivalent (string, bool, counter, int, list); convert it by hand"
                ));
            }
        };
        table.insert("type", value(var_type));
        let written_list = table.get("initial").and_then(Item::as_array).is_some();
        let raw: Option<String> = table
            .get("initial")
            .and_then(Item::as_value)
            .and_then(render_scalar);
        let initial: Value = match (var_type, raw) {
            ("list", _) if written_list => table
                .get("initial")
                .and_then(Item::as_value)
                .cloned()
                .unwrap_or_else(|| Value::Array(Array::new())),
            ("list", Some(text)) => Value::Array(list_initial(&text).into_iter().collect()),
            ("list", None) => Value::Array(Array::new()),
            ("string", Some(text)) => text.into(),
            ("string", None) => "".into(),
            ("counter", Some(text)) => {
                let n = text.trim().parse::<i64>().ok().filter(|n| *n >= 0);
                if n.is_none() {
                    self.note(format!(
                        "{scope}: initial `{text}` read as 0, as the runtime did"
                    ));
                }
                n.unwrap_or(0).into()
            }
            ("counter", None) => 0.into(),
            ("int", Some(text)) => text
                .trim()
                .parse::<i64>()
                .map_err(|_| {
                    format!("{scope}: initial `{text}` is not an integer; convert it by hand")
                })?
                .into(),
            ("int", None) => 0.into(),
            (_, Some(text)) => {
                let lower = text.trim().to_ascii_lowercase();
                let truth = matches!(lower.as_str(), "true" | "1" | "yes" | "on");
                if lower != "true" && lower != "false" {
                    self.note(format!(
                        "{scope}: initial `{text}` read as {truth}, as the runtime did"
                    ));
                }
                truth.into()
            }
            (_, None) => false.into(),
        };
        let unchanged = table
            .get("initial")
            .and_then(Item::as_value)
            .is_some_and(|written| written.to_string().trim() == initial.to_string().trim());
        if !unchanged {
            table.insert("initial", Item::Value(initial));
        }
        if let Some(text) = string_value(table, "query_indexed") {
            match text.as_str() {
                "true" | "false" => {
                    table.insert("query_indexed", value(text == "true"));
                }
                other => {
                    table.remove("query_indexed");
                    self.note(format!(
                        "{scope}: dropped query_indexed = \"{other}\" (it was ignored)"
                    ));
                }
            }
        }
        for key in ["overflow_inline_max_bytes", "overflow_ttl_seconds"] {
            let Some(item) = table.get(key) else { continue };
            if item.as_integer().is_some_and(|n| n >= 0) {
                continue;
            }
            match item
                .as_value()
                .and_then(render_scalar)
                .and_then(|t| t.trim().parse::<i64>().ok())
                .filter(|n| *n >= 0)
            {
                Some(n) => {
                    table.insert(key, value(n));
                }
                None => {
                    table.remove(key);
                    self.note(format!(
                        "{scope}: dropped {key} (not a non-negative integer, so it was ignored)"
                    ));
                }
            }
        }
        self.known(
            scope,
            table,
            &[
                "name",
                "type",
                "initial",
                "overflow_inline_max_bytes",
                "overflow_ttl_seconds",
                "query_indexed",
            ],
        );
        Ok(())
    }

    fn liveness(&mut self, scope: &str, table: &mut dyn TableLike) -> Result<(), String> {
        stringify(table, "name");
        listify(table, "from");
        listify(table, "reaches");
        if let Some(item) = table.get("has_actions")
            && item.as_bool().is_none()
        {
            let text = item.as_value().and_then(render_scalar).unwrap_or_default();
            if text != "true" && text != "false" {
                self.note(format!(
                    "{scope}: has_actions = \"{text}\" read as false, as the old reader did"
                ));
            }
            table.insert("has_actions", value(text == "true"));
        }
        self.known(scope, table, &["name", "from", "reaches", "has_actions"]);
        Ok(())
    }

    fn webhook(&mut self, scope: &str, table: &mut dyn TableLike) -> Result<(), String> {
        if let Some(lookup) = table.remove("entity_lookup")
            && lookup.as_str() != Some("query_param")
        {
            self.note(format!(
                "{scope}: dropped entity_lookup (the id was always read from the query string)"
            ));
        }
        if let Some(param) = table.remove("entity_param") {
            let param = param.as_str().unwrap_or("entity_id").to_string();
            table.insert("entity_id", value(format!("query.{param}")));
        } else if !table.contains_key("entity_id") {
            table.insert("entity_id", value("query.entity_id"));
        }
        if let Some(extract) = table.get_mut("extract").and_then(Item::as_table_like_mut) {
            let bare: Vec<(String, String)> = extract
                .iter()
                .filter_map(|(key, item)| {
                    let source = item.as_str()?;
                    (!source.starts_with("query."))
                        .then(|| (key.to_string(), format!("query.{source}")))
                })
                .collect();
            for (key, source) in bare {
                extract.insert(&key, value(source));
            }
        }
        self.known(
            scope,
            table,
            &[
                "name",
                "path",
                "method",
                "action",
                "entity_id",
                "extract",
                "signature",
                "hmac_secret",
                "hmac_header",
            ],
        );
        Ok(())
    }

    fn state_timeout(&mut self, scope: &str, table: &mut dyn TableLike) -> Result<(), String> {
        self.args(scope, table, false)?;
        self.known(
            scope,
            table,
            &[
                "state",
                "after_seconds",
                "on_timeout",
                "max_occurrences",
                "reset_on",
                "fire_now_on",
                "args",
            ],
        );
        Ok(())
    }
}

/// The spelling of a scalar value; `None` for arrays and tables.
fn render_scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.value().clone()),
        Value::Integer(n) => Some(n.value().to_string()),
        Value::Float(f) => Some(f.value().to_string()),
        Value::Boolean(b) => Some(b.value().to_string()),
        Value::Datetime(d) => Some(d.value().to_string()),
        Value::Array(_) | Value::InlineTable(_) => None,
    }
}

/// The string value of `key` when it is written as a string.
fn string_value(table: &dyn TableLike, key: &str) -> Option<String> {
    table.get(key).and_then(Item::as_str).map(str::to_string)
}

/// A non-string scalar where a string belongs becomes that string.
fn stringify(table: &mut dyn TableLike, key: &str) {
    let text = match table.get(key).and_then(Item::as_value) {
        Some(Value::String(_)) | None => return,
        Some(other) => render_scalar(other),
    };
    if let Some(text) = text {
        table.insert(key, value(text));
    }
}

/// A single scalar where a list belongs becomes a one-element list; empty
/// strings (which the old reader skipped) are dropped.
fn listify(table: &mut dyn TableLike, key: &str) {
    let Some(item) = table.get_mut(key) else {
        return;
    };
    if let Some(text) = item.as_value().and_then(render_scalar) {
        *item = value(Array::from_iter([text]));
    }
    if let Some(items) = item.as_array_mut() {
        items.retain(|v| v.as_str() != Some(""));
    }
}

/// The old list-initial reading: `[]`, `[a, b]`, `["a", "b"]` or one value.
fn list_initial(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == "[]" {
        return Vec::new();
    }
    let unquote = |s: &str| s.trim().trim_matches('"').trim_matches('\'').to_string();
    match trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
        Some(inner) => inner
            .split(',')
            .map(unquote)
            .filter(|s| !s.is_empty())
            .collect(),
        None => vec![unquote(trimmed)],
    }
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
