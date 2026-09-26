//! Reading the four predicate slots: action `guard`, trigger `guard`,
//! `[[invariant]] assert` and `[[field_invariant]] assert`.
//!
//! Each slot holds one string in the [`crate::predicate`] grammar. The
//! pre-grammar forms are still accepted and lowered while specs migrate.

use super::super::field_invariant::{FieldInvariant, FieldPredicate};
use super::super::legacy::{self, LoweredInvariant};
use super::super::parser::AutomatonParseError;
use super::super::types::{ActionTrigger, Invariant, TriggerGuard};
use super::guards::parse_guard_value;
use super::values::{string, string_list};
use crate::predicate::{self, Expr};
use toml::{Table, Value};

fn invalid(slot: &str, message: impl std::fmt::Display) -> AutomatonParseError {
    AutomatonParseError::Validation(format!("{slot}: {message}"))
}

/// An action `guard`.
pub(super) fn action_guard(action: &str, value: &Value) -> Result<Expr, AutomatonParseError> {
    let slot = format!("action '{action}' guard");
    if let Value::String(source) = value
        && let Ok(expr) = predicate::parse(source)
    {
        return Ok(expr);
    }
    let mut guards = Vec::new();
    parse_guard_value(value, &mut guards).map_err(|e| invalid(&slot, e))?;
    Ok(legacy::guards_to_expr(&guards))
}

/// Every `[[invariant]]`, plus the states declared terminal by the legacy
/// `no_further_transitions` assert.
pub(super) fn invariants(doc: &Table) -> Result<(Vec<Invariant>, Vec<String>), AutomatonParseError> {
    let mut invariants = Vec::new();
    let mut terminal = Vec::new();
    let Some(Value::Array(entries)) = doc.get("invariant") else {
        return Ok((invariants, terminal));
    };
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
        let assert = string(table, "invariant", "assert")?.unwrap_or_default();
        let when = string_list(table, "invariant", "when")?;
        let slot = format!("invariant '{name}'");
        match legacy::invariant_to_expr(&when, &assert).map_err(|e| invalid(&slot, e))? {
            LoweredInvariant::Assert(assert) => invariants.push(Invariant { name, assert }),
            LoweredInvariant::Terminal(states) => {
                for state in states {
                    if !terminal.contains(&state) {
                        terminal.push(state);
                    }
                }
            }
            LoweredInvariant::Dropped(expr) => {
                tracing::warn!(invariant = %name, %expr, "history predicates are no longer supported; invariant dropped");
            }
        }
    }
    Ok((invariants, terminal))
}

/// Every `[[field_invariant]]`.
pub(super) fn field_invariants(doc: &Table) -> Result<Vec<FieldInvariant>, AutomatonParseError> {
    #[derive(serde::Deserialize)]
    struct Legacy {
        name: String,
        when: FieldPredicate,
        require: FieldPredicate,
        #[serde(default)]
        message: Option<String>,
    }
    let Some(Value::Array(entries)) = doc.get("field_invariant") else {
        return Ok(Vec::new());
    };
    let toml_err = |e: toml::de::Error| AutomatonParseError::Toml(format!("field_invariant: {e}"));
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.get("assert").is_some() {
            out.push(entry.clone().try_into().map_err(toml_err)?);
            continue;
        }
        let old: Legacy = entry.clone().try_into().map_err(toml_err)?;
        let assert = legacy::field_invariant_to_expr(&old.when, &old.require)
            .map_err(|e| invalid(&format!("field_invariant '{}'", old.name), e))?;
        out.push(FieldInvariant {
            name: old.name,
            assert,
            message: old.message,
        });
    }
    Ok(out)
}

/// An action's `[[action.triggers]]`.
pub(super) fn triggers(action: &Table) -> Result<Vec<ActionTrigger>, AutomatonParseError> {
    let Some(Value::Array(entries)) = action.get("triggers") else {
        return match action.get("triggers") {
            None => Ok(Vec::new()),
            Some(_) => Err(AutomatonParseError::Toml(
                "action metadata: 'triggers' must be written as [[action.triggers]]".into(),
            )),
        };
    };
    let mut out = Vec::with_capacity(entries.len());
    for entry in entries {
        let mut entry = entry.clone();
        if let Some(Value::Table(table)) = entry.get_mut("guard").cloned().as_ref()
            && let Value::Table(trigger) = &mut entry
        {
            let old: TriggerGuard = Value::Table(table.clone()).try_into().map_err(
                |e: toml::de::Error| AutomatonParseError::Toml(format!("action metadata: {e}")),
            )?;
            let expr = legacy::trigger_guard_to_expr(&old)
                .map_err(|e| invalid("trigger guard", e))?;
            trigger.insert("guard".into(), Value::String(expr.to_string()));
        }
        out.push(
            entry
                .try_into()
                .map_err(|e: toml::de::Error| AutomatonParseError::Toml(format!("action metadata: {e}")))?,
        );
    }
    Ok(out)
}
