//! Rewrite old effects and `[[integration]]` blocks: verb and table effects
//! become effect statements, `trigger` effects and `[[integration]]` blocks
//! become `[[action.triggers]]`, and `emit` effects are dropped.

use std::collections::BTreeMap;

use toml_edit::{ArrayOfTables, DocumentMut, InlineTable, Item, Table, value};

use super::effects::{Effect as Old, parse_effect_value};
use super::migrate::to_toml;
use crate::automaton::toml_parser::values::any_string;
use crate::predicate::{VarKind, parse_effect};

/// A `[[integration]]` block.
struct Integration {
    name: String,
    trigger: String,
    kind: String,
    module: Option<String>,
    on_success: Option<String>,
    on_failure: Option<String>,
    llm: bool,
    config: BTreeMap<String, String>,
}

/// Where a `trigger <name>` effect can point: `[[integration]]` blocks (and
/// whether some action fired each), and external inline triggers by name
/// with the action declaring each.
struct TriggerSources {
    integrations: Vec<Integration>,
    used: Vec<bool>,
    inline: BTreeMap<String, Vec<(String, Table)>>,
}

/// Convert every action's effects, and fold `[[integration]]` blocks into the
/// actions whose `trigger` effects fired them.
pub(super) fn migrate_effects(
    doc: &mut DocumentMut,
    kinds: &BTreeMap<String, VarKind>,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let integrations = take_integrations(doc)?;
    let mut sources = TriggerSources {
        used: vec![false; integrations.len()],
        integrations,
        inline: inline_triggers(doc),
    };

    let Some(actions) = doc.get_mut("action").and_then(Item::as_array_of_tables_mut) else {
        return report_unused(&sources, notes);
    };
    for action in actions.iter_mut() {
        let name = action
            .get("name")
            .and_then(Item::as_str)
            .unwrap_or("?")
            .to_string();
        let old = match action.get("effect") {
            None => name_heuristic(&name, kinds, notes),
            Some(item) if is_current(item) => continue,
            Some(item) => {
                let mut old = Vec::new();
                parse_effect_value(&to_toml(item)?, &mut old)
                    .map_err(|e| format!("action '{name}' effect: {e}"))?;
                old
            }
        };
        let mut statements = Vec::new();
        let mut triggers = Vec::new();
        for effect in old {
            match effect {
                Old::Emit { event } => notes.push(format!(
                    "action '{name}': dropped `emit {event}` (events are not routed; each action emits its own name)"
                )),
                Old::Trigger { name: target } => {
                    let resolved = sources.resolve(&name, &target, action, notes)?;
                    triggers.extend(resolved);
                }
                other => statements.push(statement(&name, other, kinds)?),
            }
        }
        if statements.is_empty() {
            action.remove("effect");
        } else {
            let mut array = toml_edit::Array::new();
            for statement in statements {
                array.push(statement);
            }
            action.insert("effect", value(array));
        }
        if !triggers.is_empty() {
            if action.get("triggers").is_none() {
                action.insert("triggers", Item::ArrayOfTables(ArrayOfTables::new()));
            }
            let list = action
                .get_mut("triggers")
                .and_then(Item::as_array_of_tables_mut)
                .ok_or_else(|| {
                    format!("action '{name}': 'triggers' must be [[action.triggers]]")
                })?;
            for trigger in triggers {
                list.push(trigger);
            }
        }
    }
    report_unused(&sources, notes)
}

/// Whether `effect` is already a list of effect statements.
fn is_current(item: &Item) -> bool {
    item.as_array().is_some_and(|items| {
        items.iter().all(|entry| {
            entry
                .as_str()
                .is_some_and(|text| parse_effect(text).is_ok())
        })
    })
}

/// The effects the old runtime inferred for an action with none: an action
/// named like `AddItem` / `RemoveItem` changed every counter.
fn name_heuristic(
    action: &str,
    kinds: &BTreeMap<String, VarKind>,
    notes: &mut Vec<String>,
) -> Vec<Old> {
    let lower = action.to_lowercase();
    let increment = lower.contains("additem") || lower.contains("add_item");
    let decrement = lower.contains("removeitem") || lower.contains("remove_item");
    if !increment && !decrement {
        return Vec::new();
    }
    let counters: Vec<String> = kinds
        .iter()
        .filter(|(_, kind)| **kind == VarKind::Counter)
        .map(|(name, _)| name.clone())
        .collect();
    if counters.is_empty() {
        return Vec::new();
    }
    notes.push(format!(
        "action '{action}': wrote the effects its name implied ({} every counter)",
        if increment { "increment" } else { "decrement" }
    ));
    counters
        .into_iter()
        .map(|var| {
            if increment {
                Old::Increment { var, amount: None }
            } else {
                Old::Decrement { var, amount: None }
            }
        })
        .collect()
}

/// One old state/timer/spawn effect as a statement.
fn statement(
    action: &str,
    effect: Old,
    kinds: &BTreeMap<String, VarKind>,
) -> Result<String, String> {
    let fail = |detail: String| format!("action '{action}' effect: {detail}");
    let amount = |amount: Option<String>| match amount {
        Some(param) => format!("params.{param}"),
        None => "1".to_string(),
    };
    let text = match effect {
        Old::Increment { var, amount: a } => format!("{var} += {}", amount(a)),
        Old::Decrement { var, amount: a } => format!("{var} -= {}", amount(a)),
        Old::SetCounterFromParam { var, param } => format!("{var} = params.{param}"),
        Old::SetBool { var, value } => {
            if kinds.get(&var) != Some(&VarKind::Bool) {
                return Err(fail(format!(
                    "`set {var}` writes '{var}', which is not a declared bool"
                )));
            }
            format!("{var} = {value}")
        }
        Old::ListAppend { var } => format!("append({var}, params.{var})"),
        Old::ListRemoveAt { var } => format!("remove_at({var}, params.{var}_index)"),
        Old::Schedule {
            action,
            delay_seconds,
        } => format!("schedule('{action}', {delay_seconds})"),
        Old::ScheduleAt { action, field } => format!("schedule_at('{action}', {field})"),
        Old::Spawn {
            entity_type,
            entity_id_source,
            initial_action,
            store_id_in,
            copy_fields,
        } => {
            if copy_fields.is_some() {
                return Err(fail(format!(
                    "spawn of '{entity_type}' uses copy_fields, which is gone; convert by hand"
                )));
            }
            let initial = initial_action
                .ok_or_else(|| fail(format!("spawn of '{entity_type}' has no initial_action")))?;
            let id = (entity_id_source != "{uuid}").then(|| format!("params.{entity_id_source}"));
            match (store_id_in, id) {
                (Some(field), Some(id)) => {
                    format!("spawn('{entity_type}', '{initial}', {field}, {id})")
                }
                (Some(field), None) => format!("spawn('{entity_type}', '{initial}', {field})"),
                (None, None) => format!("spawn('{entity_type}', '{initial}')"),
                (None, Some(_)) => {
                    return Err(fail(format!(
                        "spawn of '{entity_type}' takes its id from '{entity_id_source}' but stores it nowhere; add a store field by hand"
                    )));
                }
            }
        }
        Old::Emit { .. } | Old::Trigger { .. } => unreachable!("handled by the caller"),
    };
    parse_effect(&text).map_err(|e| fail(e.to_string()))?;
    Ok(text)
}

impl TriggerSources {
    /// The trigger blocks `trigger <target>` on `action` adds: none when the
    /// action already declares it; an `[[integration]]` or another action's
    /// inline trigger copied onto it; or, for an unmatched capitalized name,
    /// a platform hook.
    fn resolve(
        &mut self,
        action_name: &str,
        target: &str,
        action: &Table,
        notes: &mut Vec<String>,
    ) -> Result<Vec<Table>, String> {
        let local = action
            .get("triggers")
            .and_then(Item::as_array_of_tables)
            .is_some_and(|list| {
                list.iter()
                    .any(|t| t.get("name").and_then(Item::as_str) == Some(target))
            });
        if local {
            return Ok(Vec::new());
        }
        let matching: Vec<usize> = self
            .integrations
            .iter()
            .enumerate()
            .filter(|(_, ig)| ig.trigger == target)
            .map(|(index, _)| index)
            .collect();
        if !matching.is_empty() {
            let mut out = Vec::new();
            for index in matching {
                self.used[index] = true;
                out.extend(integration_trigger(&self.integrations[index], notes)?);
            }
            return Ok(out);
        }
        if let Some(owners) = self.inline.get(target) {
            let [(owner, table)] = owners.as_slice() else {
                return Err(format!(
                    "action '{action_name}': `trigger {target}` is ambiguous across actions {:?}",
                    owners.iter().map(|(owner, _)| owner).collect::<Vec<_>>()
                ));
            };
            notes.push(format!(
                "action '{action_name}': copied trigger '{target}' from action '{owner}'"
            ));
            return Ok(vec![detached(table)]);
        }
        if target
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_uppercase())
        {
            let mut table = Table::new();
            table.insert("name", value(target));
            table.insert("kind", value("hook"));
            table.insert("hook", value(target));
            return Ok(vec![table]);
        }
        Err(format!(
            "action '{action_name}': `trigger {target}` matches no [[integration]], inline trigger or platform hook"
        ))
    }
}

/// The `[[action.triggers]]` block for an integration, or `None` for a
/// webhook integration without a URL (the old runtime never fired those).
fn integration_trigger(ig: &Integration, notes: &mut Vec<String>) -> Result<Option<Table>, String> {
    let mut config = ig.config.clone();
    let mut table = Table::new();
    table.insert("name", value(ig.name.as_str()));
    match ig.kind.as_str() {
        "wasm" => {
            let module = ig.module.as_deref().ok_or_else(|| {
                format!("integration '{}' is type 'wasm' but has no module", ig.name)
            })?;
            table.insert("kind", value("wasm"));
            table.insert("module", value(module));
        }
        "adapter" => {
            let adapter = config
                .remove("adapter")
                .or_else(|| config.remove("adapter_type"))
                .ok_or_else(|| {
                    format!(
                        "integration '{}' is type 'adapter' but has no adapter",
                        ig.name
                    )
                })?;
            table.insert("kind", value("adapter"));
            table.insert("adapter", value(adapter));
        }
        "webhook" => {
            let Some(url) = config.remove("url") else {
                notes.push(format!(
                    "integration '{}' dropped: a webhook without a url never fired",
                    ig.name
                ));
                return Ok(None);
            };
            table.insert("kind", value("webhook"));
            table.insert("url", value(url));
            let method = config.remove("method").unwrap_or_else(|| "POST".into());
            table.insert("method", value(method));
        }
        other => {
            return Err(format!(
                "integration '{}' has type '{other}', which has no [[action.triggers]] kind; convert it by hand",
                ig.name
            ));
        }
    }
    if let Some(on_success) = &ig.on_success {
        table.insert("on_success", value(on_success.as_str()));
    }
    if let Some(on_failure) = &ig.on_failure {
        table.insert("on_failure", value(on_failure.as_str()));
    }
    if ig.llm {
        table.insert("llm", value(true));
    }
    if !config.is_empty() {
        let mut inline = InlineTable::new();
        for (key, text) in config {
            inline.insert(&key, text.into());
        }
        table.insert("config", value(inline));
    }
    Ok(Some(table))
}

/// A copy of `table` without its document position, so it renders under the
/// action it is added to. Sub-tables become inline tables.
fn detached(table: &Table) -> Table {
    let mut copy = Table::new();
    for (key, item) in table.iter() {
        match item {
            Item::Table(sub) => {
                copy.insert(key, value(sub.clone().into_inline_table()));
            }
            other => {
                copy.insert(key, other.clone());
            }
        }
    }
    copy
}

/// Remove `[[integration]]` from the document and read each block.
fn take_integrations(doc: &mut DocumentMut) -> Result<Vec<Integration>, String> {
    let Some(item) = doc.remove("integration") else {
        return Ok(Vec::new());
    };
    let toml::Value::Array(blocks) = to_toml(&item)? else {
        return Err("'integration' must be written as [[integration]]".into());
    };
    blocks
        .iter()
        .map(|block| {
            let table = block
                .as_table()
                .ok_or("'integration' must be written as [[integration]]")?;
            let text = |key: &str| table.get(key).map(any_string);
            let mut config = BTreeMap::new();
            for (key, entry) in table {
                match key.as_str() {
                    "name" | "trigger" | "type" | "module" | "on_success" | "on_failure"
                    | "llm" => {}
                    "config" if entry.is_table() => {
                        for (config_key, config_value) in entry.as_table().into_iter().flatten() {
                            config.insert(config_key.clone(), any_string(config_value));
                        }
                    }
                    _ => {
                        config.insert(key.clone(), any_string(entry));
                    }
                }
            }
            Ok(Integration {
                name: text("name").unwrap_or_default(),
                trigger: text("trigger").unwrap_or_default(),
                kind: text("type").unwrap_or_else(|| "webhook".into()),
                module: text("module"),
                on_success: text("on_success"),
                on_failure: text("on_failure"),
                llm: table.get("llm").and_then(toml::Value::as_bool) == Some(true)
                    || text("llm").as_deref() == Some("true"),
                config,
            })
        })
        .collect()
}

/// External inline triggers by name, with the action declaring each.
fn inline_triggers(doc: &DocumentMut) -> BTreeMap<String, Vec<(String, Table)>> {
    let mut found: BTreeMap<String, Vec<(String, Table)>> = BTreeMap::new();
    let Some(actions) = doc.get("action").and_then(Item::as_array_of_tables) else {
        return found;
    };
    for action in actions.iter() {
        let owner = action.get("name").and_then(Item::as_str).unwrap_or("?");
        let Some(triggers) = action.get("triggers").and_then(Item::as_array_of_tables) else {
            continue;
        };
        for trigger in triggers.iter() {
            let external = matches!(
                trigger.get("kind").and_then(Item::as_str),
                Some("wasm" | "adapter" | "webhook")
            );
            if let (true, Some(name)) = (external, trigger.get("name").and_then(Item::as_str)) {
                found
                    .entry(name.to_string())
                    .or_default()
                    .push((owner.to_string(), trigger.clone()));
            }
        }
    }
    found
}

fn report_unused(sources: &TriggerSources, notes: &mut Vec<String>) -> Result<(), String> {
    for (ig, used) in sources.integrations.iter().zip(&sources.used) {
        if !used {
            notes.push(format!(
                "integration '{}' dropped: no action fired `trigger {}`",
                ig.name, ig.trigger
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[path = "migrate_effects_test.rs"]
mod tests;
