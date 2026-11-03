//! `[[action]]`, its parameters and `[[action.triggers]]`, and `args`.

use toml_edit::{Array, InlineTable, Item, Table, TableLike, Value, value};

use super::{Pass, listify, stringify};
use crate::predicate::{CmpOp, Expr, Literal, Operand, is_keyword};

impl Pass<'_> {
    pub(super) fn action(&mut self, scope: &str, table: &mut dyn TableLike) -> Result<(), String> {
        for key in ["kind", "to", "hint"] {
            stringify(table, key);
        }
        listify(table, "from");
        if let Some(kind) = table.get("kind").and_then(Item::as_str).map(str::to_string) {
            let lower = kind.to_ascii_lowercase();
            let current = if matches!(
                lower.as_str(),
                "input" | "internal" | "output" | "composite"
            ) {
                lower
            } else {
                self.note(format!(
                    "{scope}: kind `{kind}` read as internal, as the kernel did"
                ));
                "internal".to_string()
            };
            if current != kind {
                table.insert("kind", value(current));
            }
        }
        let composite = table.get("kind").and_then(Item::as_str) == Some("composite");
        if let Some(item) = table.get("record_parent_event") {
            let flag = match item.as_bool() {
                Some(flag) => Some(flag),
                None => match item.as_str() {
                    Some("true") => Some(true),
                    Some("false") => Some(false),
                    _ => None,
                },
            };
            let written_as_bool = item.as_bool().is_some();
            match flag {
                Some(_) if composite && written_as_bool => {}
                Some(flag) if composite => {
                    table.insert("record_parent_event", value(flag));
                }
                Some(_) => {
                    table.remove("record_parent_event");
                    self.note(format!(
                        "{scope}: dropped record_parent_event (only composite actions read it)"
                    ));
                }
                None => {
                    table.remove("record_parent_event");
                    self.note(format!("{scope}: dropped record_parent_event (not true or false, so it read as true)"));
                }
            }
        }
        if !composite {
            for key in ["cedar_gate", "sub_writes"] {
                if table.remove(key).is_some() {
                    self.note(format!(
                        "{scope}: dropped {key} (only composite actions read it)"
                    ));
                }
            }
        } else if let Some(gates) = table
            .get_mut("cedar_gate")
            .and_then(Item::as_array_of_tables_mut)
            && gates.len() > 1
        {
            while gates.len() > 1 {
                gates.remove(gates.len() - 1);
            }
            self.note(format!(
                "{scope}: kept the first cedar_gate (only the first was read)"
            ));
        }
        self.params(scope, table)?;
        if let Some(triggers) = table.get_mut("triggers") {
            let entries: Vec<&mut dyn TableLike> = match triggers {
                Item::ArrayOfTables(tables) => {
                    tables.iter_mut().map(|t| t as &mut dyn TableLike).collect()
                }
                Item::Value(Value::Array(values)) => values
                    .iter_mut()
                    .filter_map(|v| v.as_inline_table_mut().map(|t| t as &mut dyn TableLike))
                    .collect(),
                _ => Vec::new(),
            };
            for trigger in entries {
                let name = trigger
                    .get("name")
                    .and_then(Item::as_str)
                    .unwrap_or("?")
                    .to_string();
                self.trigger(&format!("{scope} trigger '{name}'"), trigger)?;
            }
        }
        self.known(
            scope,
            table,
            &[
                "name",
                "kind",
                "from",
                "to",
                "guard",
                "effect",
                "params",
                "constraints",
                "hint",
                "says",
                "record_parent_event",
                "triggers",
                "cedar_gate",
                "sub_writes",
            ],
        );
        Ok(())
    }

    fn params(&mut self, scope: &str, table: &mut dyn TableLike) -> Result<(), String> {
        let Some(item) = table.get_mut("params") else {
            return Ok(());
        };
        if let Some(name) = item.as_str().map(str::to_string) {
            *item = value(Array::from_iter([name]));
        }
        let Some(params) = item.as_array_mut() else {
            return Ok(());
        };
        let before = params.len();
        params.retain(|param| match param {
            Value::String(name) => !name.value().is_empty(),
            Value::InlineTable(t) => t
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|n| !n.is_empty()),
            _ => true,
        });
        if params.len() != before {
            self.note(format!(
                "{scope}: dropped parameters without a name (they were never read)"
            ));
        }
        for param in params.iter_mut() {
            let Some(typed) = param.as_inline_table_mut() else {
                continue;
            };
            let Some(old) = typed
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
            else {
                continue;
            };
            let current = match old.as_str() {
                "string" | "status" => "string",
                "int" | "integer" => "int",
                "counter" | "uint64" => "counter",
                "bool" => "bool",
                "list" | "set" => "list",
                other => {
                    return Err(format!(
                        "{scope}: parameter type `{other}` has no current equivalent; convert it by hand"
                    ));
                }
            };
            if current != old {
                typed.insert("type", current.into());
            }
        }
        Ok(())
    }

    fn trigger(&mut self, scope: &str, table: &mut dyn TableLike) -> Result<(), String> {
        let kind = table
            .get("kind")
            .and_then(Item::as_str)
            .unwrap_or_default()
            .to_string();
        if let Some(to_state) = table
            .remove("to_state")
            .and_then(|i| i.as_str().map(str::to_string))
        {
            if kind == "entity" {
                let mut parts = vec![Expr::Compare {
                    lhs: Operand::Status,
                    op: CmpOp::Eq,
                    rhs: Operand::Lit(Literal::Str(checked_literal(scope, &to_state)?.to_string())),
                }];
                if let Some(text) = table.get("guard").and_then(Item::as_str) {
                    parts.push(
                        crate::predicate::parse(text).map_err(|e| format!("{scope} guard: {e}"))?,
                    );
                }
                table.insert("guard", value(Expr::and(parts).to_string()));
            } else {
                self.note(format!(
                    "{scope}: dropped to_state (only entity triggers read it)"
                ));
            }
        }
        if let Some(adapter_type) = table.remove("adapter_type") {
            if table.contains_key("adapter") {
                self.note(format!(
                    "{scope}: dropped adapter_type (adapter was read first)"
                ));
            } else {
                table.insert("adapter", adapter_type);
            }
        }
        if let Some(resolver) = table
            .get_mut("resolve_target")
            .and_then(Item::as_table_like_mut)
        {
            if let Some(kind) = resolver.remove("type") {
                resolver.insert("kind", kind);
            }
            if resolver.get("kind").and_then(Item::as_str) == Some("field")
                && let Some(field) = resolver.remove("field")
            {
                resolver.insert("id_field", field);
            }
        }
        self.args(scope, table, true)?;

        let allowed: &[&str] = match kind.as_str() {
            "entity" => &[
                "principal",
                "guard",
                "liveness",
                "drop_ok",
                "target_entity",
                "target_action",
                "args",
                "resolve_target",
            ],
            "wasm" => &["module", "on_success", "on_failure", "config", "llm"],
            "adapter" => &["adapter", "on_success", "on_failure", "config", "llm"],
            "webhook" => &[
                "url",
                "method",
                "headers",
                "body_template",
                "on_success",
                "on_failure",
                "config",
            ],
            "hook" => &["hook"],
            _ => return Ok(()),
        };
        let every = [
            "principal",
            "guard",
            "liveness",
            "drop_ok",
            "llm",
            "target_entity",
            "target_action",
            "args",
            "resolve_target",
            "module",
            "on_success",
            "on_failure",
            "config",
            "adapter",
            "url",
            "method",
            "headers",
            "body_template",
            "hook",
        ];
        let foreign: Vec<String> = table
            .iter()
            .filter(|(key, item)| {
                every.contains(key) && !allowed.contains(key) && !is_default(key, item)
            })
            .map(|(key, _)| key.to_string())
            .collect();
        for key in foreign {
            table.remove(&key);
            self.note(format!(
                "{scope}: dropped `{key}` ({kind} triggers do not read it)"
            ));
        }
        let mut keys = vec!["name", "kind"];
        keys.extend(every);
        self.known(scope, table, &keys);
        Ok(())
    }

    /// `params` (literals) and, on triggers, `params_from` (field names)
    /// become one `args` table of expression strings.
    pub(super) fn args(
        &mut self,
        scope: &str,
        table: &mut dyn TableLike,
        from_fields: bool,
    ) -> Result<(), String> {
        let literals = table.remove("params");
        let fields = if from_fields {
            table.remove("params_from")
        } else {
            None
        };
        if literals.is_none() && fields.is_none() {
            return Ok(());
        }
        let mut args: Vec<(String, String)> = Vec::new();
        if let Some(literals) = &literals {
            let map = literals
                .as_table_like()
                .ok_or_else(|| format!("{scope}: params must be a table"))?;
            for (key, item) in map.iter() {
                let text = match item.as_value() {
                    Some(Value::String(s)) => format!("'{}'", checked_literal(scope, s.value())?),
                    Some(Value::Integer(n)) => n.value().to_string(),
                    Some(Value::Boolean(b)) => b.value().to_string(),
                    _ => {
                        return Err(format!(
                            "{scope}: params.{key} has no literal spelling; convert it by hand"
                        ));
                    }
                };
                args.push((key.to_string(), text));
            }
        }
        if let Some(fields) = &fields {
            let map = fields
                .as_table_like()
                .ok_or_else(|| format!("{scope}: params_from must be a table"))?;
            for (key, item) in map.iter() {
                let field = item
                    .as_str()
                    .ok_or_else(|| format!("{scope}: params_from.{key} must name a field"))?;
                let is_name = field
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                    && field.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && !is_keyword(field);
                if !is_name {
                    return Err(format!(
                        "{scope}: params_from.{key} = \"{field}\" is not a field name; convert it by hand"
                    ));
                }
                args.push((key.to_string(), field.to_string()));
            }
        }
        // Keep the layout the author used: a `[... .args]` section when either
        // source was one, an inline table otherwise.
        let section = [&literals, &fields]
            .into_iter()
            .flatten()
            .find_map(|item| item.as_table().map(Table::position));
        match section {
            Some(position) => {
                let mut out = Table::new();
                if let Some(position) = position {
                    out.set_position(position);
                }
                for (key, text) in args {
                    out.insert(&key, value(text));
                }
                table.insert("args", Item::Table(out));
            }
            None => {
                let mut out = InlineTable::new();
                for (key, text) in args {
                    out.insert(&key, text.into());
                }
                table.insert("args", value(out));
            }
        }
        Ok(())
    }
}

/// A string literal the expression grammar can hold (no `'`).
fn checked_literal<'s>(scope: &str, text: &'s str) -> Result<&'s str, String> {
    if text.contains('\'') {
        return Err(format!(
            "{scope}: `{text}` contains ', which a string literal cannot hold; convert it by hand"
        ));
    }
    Ok(text)
}

/// Whether a trigger key holds its default (so it was never meaningful).
fn is_default(key: &str, item: &Item) -> bool {
    match key {
        "liveness" => item.as_str() == Some("best_effort"),
        "drop_ok" | "llm" => item.as_bool() == Some(false),
        "config" | "headers" | "args" => item.as_table_like().is_some_and(|t| t.is_empty()),
        _ => false,
    }
}
