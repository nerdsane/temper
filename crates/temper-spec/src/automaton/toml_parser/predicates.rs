//! Reading the four predicate slots: action `guard`, trigger `guard`,
//! `[[invariant]] assert` and `[[field_invariant]] assert`. Each holds one
//! string in the [`crate::predicate`] grammar.

use super::super::field_invariant::FieldInvariant;
use super::super::legacy;
use super::super::parser::AutomatonParseError;
use super::super::types::{ActionTrigger, Invariant};
use super::values::string;
use crate::predicate::{self, Expr};
use toml::{Table, Value};

/// Appended to errors for specs still written in the old predicate syntax.
const MIGRATE_HINT: &str =
    "this is the old predicate syntax; convert the spec with `temper migrate-predicates`";

fn invalid(slot: &str, message: impl std::fmt::Display) -> AutomatonParseError {
    AutomatonParseError::Validation(format!("{slot}: {message}"))
}

/// One predicate string.
fn expression(slot: &str, value: &Value, legacy: bool) -> Result<Expr, AutomatonParseError> {
    let Value::String(source) = value else {
        return Err(invalid(
            slot,
            format!("must be an expression string; {MIGRATE_HINT}"),
        ));
    };
    predicate::parse(source).map_err(|e| {
        if legacy {
            invalid(slot, format!("{e}; {MIGRATE_HINT}"))
        } else {
            invalid(slot, e)
        }
    })
}

/// An action `guard`.
pub(super) fn action_guard(action: &str, value: &Value) -> Result<Expr, AutomatonParseError> {
    expression(
        &format!("action '{action}' guard"),
        value,
        legacy::is_legacy_guard(value),
    )
}

/// Every `[[invariant]]`.
pub(super) fn invariants(doc: &Table) -> Result<Vec<Invariant>, AutomatonParseError> {
    let Some(value) = doc.get("invariant") else {
        return Ok(Vec::new());
    };
    let Value::Array(entries) = value else {
        return Err(AutomatonParseError::Toml(
            "'invariant' must be written as [[invariant]]".into(),
        ));
    };
    let mut invariants = Vec::with_capacity(entries.len());
    for entry in entries {
        let Value::Table(table) = entry else {
            return Err(AutomatonParseError::Toml(
                "'invariant' must be written as [[invariant]]".into(),
            ));
        };
        let name = string(table, "invariant", "name")?.unwrap_or_default();
        if name.is_empty() {
            continue;
        }
        let slot = format!("invariant '{name}'");
        if table.contains_key("when") {
            return Err(invalid(
                &slot,
                format!("`when` is not a key; {MIGRATE_HINT}"),
            ));
        }
        let value = table
            .get("assert")
            .ok_or_else(|| invalid(&slot, "missing `assert`"))?;
        if let Some(text) = value.as_str()
            && matches!(
                legacy::invariant_to_expr(&[], text),
                Ok(legacy::LoweredInvariant::Terminal(_) | legacy::LoweredInvariant::Dropped(_))
            )
        {
            return Err(invalid(
                &slot,
                format!(
                    "`{text}` is not an invariant; terminal states go in `[automaton] terminal`; {MIGRATE_HINT}"
                ),
            ));
        }
        let legacy = value.as_str().is_some_and(|text| {
            predicate::parse(text).is_err() && legacy::invariant_to_expr(&[], text).is_ok()
        });
        let assert = expression(&slot, value, legacy)?;
        invariants.push(Invariant { name, assert });
    }
    Ok(invariants)
}

/// Every `[[field_invariant]]`.
pub(super) fn field_invariants(doc: &Table) -> Result<Vec<FieldInvariant>, AutomatonParseError> {
    let Some(Value::Array(entries)) = doc.get("field_invariant") else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.get("when").is_some() || entry.get("require").is_some() {
            let name = entry.get("name").and_then(Value::as_str).unwrap_or("?");
            return Err(invalid(
                &format!("field_invariant '{name}'"),
                format!("`when`/`require` are not keys, write one `assert`; {MIGRATE_HINT}"),
            ));
        }
        out.push(entry.clone().try_into().map_err(|e: toml::de::Error| {
            AutomatonParseError::Toml(format!("field_invariant: {e}"))
        })?);
    }
    Ok(out)
}

/// An action's `[[action.triggers]]`.
pub(super) fn triggers(action: &Table) -> Result<Vec<ActionTrigger>, AutomatonParseError> {
    let Some(value) = action.get("triggers") else {
        return Ok(Vec::new());
    };
    let Value::Array(entries) = value else {
        return Err(AutomatonParseError::Toml(
            "action metadata: 'triggers' must be written as [[action.triggers]]".into(),
        ));
    };
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        if let Some(guard) = entry.get("guard")
            && !guard.is_str()
        {
            let name = entry.get("name").and_then(Value::as_str).unwrap_or("?");
            return Err(invalid(
                &format!("trigger '{name}' guard"),
                format!("must be an expression string; {MIGRATE_HINT}"),
            ));
        }
        out.push(entry.clone().try_into().map_err(|e: toml::de::Error| {
            AutomatonParseError::Toml(format!("action metadata: {e}"))
        })?);
    }
    Ok(out)
}
