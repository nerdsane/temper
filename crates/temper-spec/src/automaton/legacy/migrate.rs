//! Rewrite a spec's pre-grammar predicates into the current grammar, keeping
//! the rest of the file (comments, layout, key order) as written.

use std::collections::BTreeMap;

use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

use super::field_predicate::FieldPredicate;
use super::lower::{self, LoweredInvariant};
use super::syntax::{Guard, TriggerGuard};
use crate::predicate::{CmpOp, Expr, Literal, Operand, Scope, Set, VarKind, check, unmodelable};

/// A migrated spec.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    /// The rewritten source.
    pub source: String,
    /// What changed beyond a one-to-one rewrite, for the operator to review.
    pub notes: Vec<String>,
}

/// Rewrite every predicate in `source` into the current grammar.
///
/// - action and trigger `guard`s become expression strings
/// - `[[invariant]]` `when` + `assert` becomes one `assert`;
///   `no_further_transitions` becomes `[automaton] terminal`; `ordering()` is
///   dropped; an assertion the verifier cannot model becomes a
///   `[[field_invariant]]` (checked on writes instead)
/// - `[[field_invariant]]` `when` + `require` becomes one `assert`
/// - verb and table effects become effect statements; `trigger` effects and
///   `[[integration]]` blocks become `[[action.triggers]]`; `emit` effects
///   are dropped; an action named like `AddItem` / `RemoveItem` gets the
///   counter effects its name used to imply
///
/// - string booleans and numbers become TOML values; `[[state]] initial` is
///   written in its type; `to_state` becomes part of the trigger `guard`;
///   `params`/`params_from` become `args`; the other renamed keys get their
///   current names; keys the old reader ignored are dropped (noted)
///
/// The result is parsed with the current parser before it is returned.
pub fn migrate_source(source: &str) -> Result<Migration, String> {
    let (source, notes) = rewrite(source, true)?;
    crate::automaton::parse_automaton_with_liveness(
        &source,
        crate::automaton::LivenessEnforcement::WarnOnly,
    )
    .map_err(|e| format!("migrated spec does not parse: {e}"))?;
    Ok(Migration { source, notes })
}

/// Run every conversion pass over `source`, without parsing the result.
/// `drop_unknown` removes keys and sections nothing reads.
pub(super) fn rewrite(source: &str, drop_unknown: bool) -> Result<(String, Vec<String>), String> {
    let mut doc: DocumentMut = source.parse().map_err(|e| format!("not valid TOML: {e}"))?;
    let mut notes = Vec::new();
    promote_appended_strings_to_lists(&mut doc, &mut notes)?;
    let kinds = state_kinds(&doc);

    if let Some(actions) = doc.get_mut("action").and_then(Item::as_array_of_tables_mut) {
        for action in actions.iter_mut() {
            migrate_action(action)?;
        }
    }
    super::migrate_effects::migrate_effects(&mut doc, &kinds, &mut notes)?;

    let mut terminal = Vec::new();
    let mut moved = Vec::new();
    if let Some(invariants) = doc
        .get_mut("invariant")
        .and_then(Item::as_array_of_tables_mut)
    {
        migrate_invariants(invariants, &kinds, &mut terminal, &mut moved, &mut notes)?;
        if invariants.is_empty() {
            doc.remove("invariant");
        }
    }
    if !terminal.is_empty() {
        add_terminal_states(&mut doc, terminal);
    }

    if let Some(field_invariants) = doc
        .get_mut("field_invariant")
        .and_then(Item::as_array_of_tables_mut)
    {
        for table in field_invariants.iter_mut() {
            migrate_field_invariant(table)?;
        }
    }
    if !moved.is_empty() {
        if doc.get("field_invariant").is_none() {
            doc.insert("field_invariant", Item::ArrayOfTables(ArrayOfTables::new()));
        }
        let field_invariants = doc
            .get_mut("field_invariant")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or("'field_invariant' must be written as [[field_invariant]]")?;
        for table in moved {
            field_invariants.push(table);
        }
    }

    super::migrate_names::migrate_names(&mut doc, &mut notes, drop_unknown)?;
    Ok((doc.to_string(), notes))
}

/// The old runtime let `list_append` grow a variable declared as a string with
/// an empty-list initial value (`type = "string"`, `initial = "[]"`). The
/// current reader types it, so declare those variables as lists.
fn promote_appended_strings_to_lists(
    doc: &mut DocumentMut,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let mut appended = std::collections::BTreeSet::new();
    if let Some(actions) = doc.get("action").and_then(Item::as_array_of_tables) {
        for action in actions.iter() {
            let Some(item) = action.get("effect") else {
                continue;
            };
            let mut effects = Vec::new();
            // An effect this pass cannot read is reported by migrate_effects.
            if super::effects::parse_effect_value(&to_toml(item)?, &mut effects).is_err() {
                continue;
            }
            for effect in effects {
                if let super::effects::Effect::ListAppend { var } = effect {
                    appended.insert(var);
                }
            }
        }
    }
    let Some(states) = doc.get_mut("state").and_then(Item::as_array_of_tables_mut) else {
        return Ok(());
    };
    for state in states.iter_mut() {
        let Some(name) = state.get("name").and_then(Item::as_str).map(str::to_string) else {
            continue;
        };
        let declared = state.get("type").and_then(Item::as_str).unwrap_or("string");
        let empty_initial = state.get("initial").is_none_or(|initial| {
            initial
                .as_str()
                .is_some_and(|text| matches!(text.trim(), "" | "[]"))
        });
        if appended.contains(&name) && old_kind(declared) == VarKind::Str && empty_initial {
            state.insert("type", value("list"));
            state.insert("initial", value(Array::new()));
            notes.push(format!(
                "state '{name}' is appended to; declared as a list (the old runtime treated it as one)"
            ));
        }
    }
    Ok(())
}

/// How the old reader typed a `[[state]] type`; unknown types read as strings.
fn old_kind(var_type: &str) -> VarKind {
    match var_type {
        "counter" => VarKind::Counter,
        "bool" => VarKind::Bool,
        "list" | "set" => VarKind::List,
        "int" | "integer" | "float" | "number" => VarKind::Num,
        _ => VarKind::Str,
    }
}

fn state_kinds(doc: &DocumentMut) -> BTreeMap<String, VarKind> {
    let mut kinds = BTreeMap::new();
    if let Some(states) = doc.get("state").and_then(Item::as_array_of_tables) {
        for state in states.iter() {
            let name = state.get("name").and_then(Item::as_str);
            let var_type = state.get("type").and_then(Item::as_str).unwrap_or("string");
            if let Some(name) = name {
                kinds.insert(name.to_string(), old_kind(var_type));
            }
        }
    }
    kinds
}

/// An inline value or sub-table as a `toml::Value`, for the legacy readers.
pub(super) fn to_toml(item: &Item) -> Result<toml::Value, String> {
    let text = match item {
        Item::Value(v) => format!("v = {v}"),
        // Inline, so nested sub-tables (`[action.triggers.guard.guard]`) come along.
        Item::Table(t) => format!("v = {}", t.clone().into_inline_table()),
        // `[[action.guard]]`: an array of sub-tables, read as an array of
        // inline tables.
        Item::ArrayOfTables(tables) => {
            let mut array = Array::new();
            for table in tables.iter() {
                array.push(toml_edit::Value::InlineTable(
                    table.clone().into_inline_table(),
                ));
            }
            format!("v = {array}")
        }
        Item::None => return Err("missing value".into()),
    };
    let mut table: toml::Table = text.parse().map_err(|e| format!("{e}"))?;
    table.remove("v").ok_or_else(|| "missing value".to_string())
}

fn migrate_action(action: &mut Table) -> Result<(), String> {
    let name = action
        .get("name")
        .and_then(Item::as_str)
        .unwrap_or("?")
        .to_string();
    if let Some(item) = action.get("guard") {
        let expr = legacy_guard(item).map_err(|e| format!("action '{name}' guard: {e}"))?;
        set_or_remove(action, "guard", expr);
    }
    if let Some(triggers) = action
        .get_mut("triggers")
        .and_then(Item::as_array_of_tables_mut)
    {
        for trigger in triggers.iter_mut() {
            let Some(item) = trigger.get("guard") else {
                continue;
            };
            if item.as_str().is_some() {
                continue;
            }
            let old: TriggerGuard = to_toml(item)?
                .try_into()
                .map_err(|e: toml::de::Error| format!("action '{name}' trigger guard: {e}"))?;
            let expr = lower::trigger_guard_to_expr(&old)
                .map_err(|e| format!("action '{name}' trigger guard: {e}"))?;
            trigger.insert("guard", value(simplify(expr).to_string()));
        }
    }
    Ok(())
}

fn legacy_guard(item: &Item) -> Result<Expr, String> {
    if let Some(text) = item.as_str()
        && let Ok(expr) = crate::predicate::parse(text)
    {
        return Ok(expr);
    }
    let mut guards: Vec<Guard> = Vec::new();
    super::guard_syntax::parse_guard_value(&to_toml(item)?, &mut guards)
        .map_err(|e| e.to_string())?;
    Ok(lower::guards_to_expr(&guards))
}

fn migrate_invariants(
    invariants: &mut ArrayOfTables,
    kinds: &BTreeMap<String, VarKind>,
    terminal: &mut Vec<String>,
    moved: &mut Vec<Table>,
    notes: &mut Vec<String>,
) -> Result<(), String> {
    let mut keep = Vec::new();
    for table in invariants.iter() {
        let name = table
            .get("name")
            .and_then(Item::as_str)
            .unwrap_or("?")
            .to_string();
        let assert = table
            .get("assert")
            .and_then(Item::as_str)
            .unwrap_or_default();
        let when: Vec<String> = table
            .get("when")
            .and_then(Item::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        match lower::invariant_to_expr(&when, assert)
            .map_err(|e| format!("invariant '{name}': {e}"))?
        {
            LoweredInvariant::Assert(expr) => {
                let expr = simplify(expr);
                let mut table = table.clone();
                table.remove("when");
                // Move only assertions over declared values the verifier does
                // not model (strings, numbers). An undeclared name is likely a
                // typo: leave it, so the converted spec reports it.
                let declared = check(&expr, Scope::State(kinds)).is_ok();
                if declared && unmodelable(&expr, kinds).is_some() {
                    notes.push(format!(
                        "invariant '{name}' reads values the verifier cannot model; moved to [[field_invariant]] (checked on writes)"
                    ));
                    table.insert("assert", value(expr.to_string()));
                    moved.push(table);
                } else {
                    table.insert("assert", value(expr.to_string()));
                    keep.push(table);
                }
            }
            LoweredInvariant::Terminal(states) => {
                for state in states {
                    if !terminal.contains(&state) {
                        terminal.push(state);
                    }
                }
            }
            LoweredInvariant::Dropped(expr) => {
                notes.push(format!(
                    "invariant '{name}' ({expr}) dropped: history predicates are not supported"
                ));
            }
        }
    }
    invariants.clear();
    for table in keep {
        invariants.push(table);
    }
    Ok(())
}

fn add_terminal_states(doc: &mut DocumentMut, states: Vec<String>) {
    if doc.get("automaton").is_none() {
        doc.insert("automaton", Item::Table(Table::new()));
    }
    let Some(automaton) = doc.get_mut("automaton").and_then(Item::as_table_mut) else {
        return;
    };
    let mut all: Vec<String> = automaton
        .get("terminal")
        .and_then(Item::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for state in states {
        if !all.contains(&state) {
            all.push(state);
        }
    }
    let mut array = Array::new();
    for state in all {
        array.push(state);
    }
    automaton.insert("terminal", value(array));
}

fn migrate_field_invariant(table: &mut Table) -> Result<(), String> {
    if table.get("assert").is_some() {
        return Ok(());
    }
    let name = table
        .get("name")
        .and_then(Item::as_str)
        .unwrap_or("?")
        .to_string();
    let predicate = |key: &str| -> Result<FieldPredicate, String> {
        let item = table
            .get(key)
            .ok_or_else(|| format!("field_invariant '{name}' has no `{key}`"))?;
        to_toml(item)?
            .try_into()
            .map_err(|e: toml::de::Error| format!("field_invariant '{name}' {key}: {e}"))
    };
    let expr = lower::field_invariant_to_expr(&predicate("when")?, &predicate("require")?)
        .map_err(|e| format!("field_invariant '{name}': {e}"))?;
    table.remove("when");
    table.remove("require");
    table.insert("assert", value(simplify(expr).to_string()));
    Ok(())
}

fn set_or_remove(table: &mut Table, key: &str, expr: Expr) {
    let expr = simplify(expr);
    if expr.is_always() {
        table.remove(key);
    } else {
        table.insert(key, value(expr.to_string()));
    }
}

/// Exact-equivalence tidying for readable output. Only rewrites operands
/// that are plain names, where `!`, `!=` and `not in` coincide (they differ
/// for a related-entity status, which is checked against every entity).
pub fn simplify(expr: Expr) -> Expr {
    let plain = |operand: &Operand| matches!(operand, Operand::Var(_) | Operand::Status);
    match expr {
        Expr::Not(inner) => match simplify(*inner) {
            Expr::Not(inner) => *inner,
            Expr::Compare { lhs, op, rhs } if plain(&lhs) && op == CmpOp::Eq => Expr::Compare {
                lhs,
                op: CmpOp::Ne,
                rhs,
            },
            Expr::In {
                value,
                set,
                negated,
            } if plain(&value) => Expr::In {
                value,
                set,
                negated: !negated,
            },
            other => Expr::Not(Box::new(other)),
        },
        Expr::And(parts) => Expr::and(parts.into_iter().map(simplify).collect()),
        Expr::Or(parts) => {
            let parts: Vec<Expr> = parts.into_iter().map(simplify).collect();
            equalities_to_in(&parts).unwrap_or(Expr::Or(parts))
        }
        Expr::Implies(a, b) => Expr::Implies(Box::new(simplify(*a)), Box::new(simplify(*b))),
        other => other,
    }
}

/// `x == a || x == b || ...` over one plain name as `x in [a, b, ...]`.
fn equalities_to_in(parts: &[Expr]) -> Option<Expr> {
    let mut name: Option<&Operand> = None;
    let mut literals: Vec<Literal> = Vec::new();
    for part in parts {
        let Expr::Compare {
            lhs: lhs @ (Operand::Var(_) | Operand::Status),
            op: CmpOp::Eq,
            rhs: Operand::Lit(literal),
        } = part
        else {
            return None;
        };
        if name.is_some_and(|name| name != lhs) {
            return None;
        }
        name = Some(lhs);
        literals.push(literal.clone());
    }
    Some(Expr::In {
        value: name?.clone(),
        set: Set::List(literals),
        negated: false,
    })
}
