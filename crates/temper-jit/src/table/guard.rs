//! Guard evaluation.
//!
//! A guard is a [`temper_spec::predicate::Expr`] on a
//! [`super::types::TransitionRule`], evaluated against an [`EvalContext`]
//! before the transition fires. [`check`] is the hot path; [`check_detailed`]
//! runs only on rejection and names the failing sub-expression so the server
//! can render a self-heal error (ADR-0151).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use temper_spec::predicate::{Env, Expr, Name, Truth, Val, eval, explain};

use super::action_contract::InitialValues;

/// The failing part of a rejected guard (ADR-0151).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuardFailure {
    /// The smallest failing sub-expression, in canonical form.
    pub expr: String,
    /// Each name that sub-expression reads, with the value found.
    pub found: Vec<(String, String)>,
}

/// Runtime values a guard can read.
#[derive(Debug, Clone, Default)]
pub struct EvalContext {
    /// Named counter values (e.g., "items" -> 2, "review_cycles" -> 1).
    pub counters: BTreeMap<String, usize>,
    /// Named boolean values (e.g., "assignee_set" -> true).
    pub booleans: BTreeMap<String, bool>,
    /// Named list values (e.g., "tags" -> ["urgent", "review"]).
    pub lists: BTreeMap<String, Vec<String>>,
    /// Entity fields guards read directly (string state variables, reference
    /// ids). Only names a guard references need to be present.
    pub fields: BTreeMap<String, serde_json::Value>,
    /// Related entities' statuses, keyed by `(entity_type, id_field)`.
    /// A reference absent from the map is unset.
    pub related: RelatedMap,
}

/// Related entities' statuses keyed by `(entity_type, id_field)`.
pub type RelatedMap = BTreeMap<(String, String), Related>;

/// The statuses of the entities one reference field points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Related {
    /// One status per id; `None` for an id whose entity was not found. Empty
    /// when the reference is unset.
    Statuses(Vec<Option<String>>),
    /// Not resolved (lookup budget exhausted). Reads as unknown, so no guard
    /// that depends on it can pass.
    Unresolved,
}

struct GuardEnv<'a> {
    status: &'a str,
    ctx: &'a EvalContext,
    declared: &'a InitialValues,
}

impl Env for GuardEnv<'_> {
    fn status(&self) -> Val<'_> {
        Val::Str(self.status)
    }

    fn var(&self, name: &str) -> Val<'_> {
        let ctx = self.ctx;
        if let Some(value) = ctx.counters.get(name) {
            return Val::Num(*value as f64);
        }
        if let Some(value) = ctx.booleans.get(name) {
            return Val::Bool(*value);
        }
        if let Some(value) = ctx.lists.get(name) {
            return Val::Strs(value);
        }
        if let Some(value) = ctx.fields.get(name) {
            return Val::json(value);
        }
        // A declared variable missing from the context reads as its type's
        // zero value, never as absent.
        if self.declared.counters.contains_key(name) {
            return Val::Num(0.0);
        }
        if self.declared.booleans.contains_key(name) {
            return Val::Bool(false);
        }
        if self.declared.lists.contains_key(name) {
            return Val::Strs(&[]);
        }
        Val::Null
    }

    fn cross_statuses(&self, entity_type: &str, id_field: &str) -> Option<Vec<Val<'_>>> {
        let key = (entity_type.to_string(), id_field.to_string());
        match self.ctx.related.get(&key) {
            Some(Related::Unresolved) => None,
            Some(Related::Statuses(statuses)) if !statuses.is_empty() => Some(
                statuses
                    .iter()
                    .map(|status| status.as_deref().map_or(Val::Null, Val::Str))
                    .collect(),
            ),
            _ => Some(vec![Val::Null]),
        }
    }
}

/// Whether `guard` holds (hot path).
pub fn check(
    guard: &Expr,
    current_state: &str,
    ctx: &EvalContext,
    declared: &InitialValues,
) -> bool {
    let env = GuardEnv {
        status: current_state,
        ctx,
        declared,
    };
    // Unknown only arises from an unresolved reference; it must not pass.
    eval(guard, &env) == Truth::True
}

/// The failing sub-expression and the values it read, or `None` when the
/// guard holds (cold rejection path).
pub fn check_detailed(
    guard: &Expr,
    current_state: &str,
    ctx: &EvalContext,
    declared: &InitialValues,
) -> Option<GuardFailure> {
    let env = GuardEnv {
        status: current_state,
        ctx,
        declared,
    };
    if eval(guard, &env) == Truth::True {
        return None;
    }
    // A guard that is unknown (an unresolved reference) has no definitely
    // failing part; report it whole.
    let failing = explain(guard, &env).unwrap_or(guard);
    let mut found: Vec<(String, String)> = Vec::new();
    failing.for_each_name(&mut |name| {
        let (label, value) = match name {
            Name::Status => ("status".to_string(), current_state.to_string()),
            Name::Var(var) => (var.to_string(), render(env.var(var))),
            Name::CrossStatus {
                entity_type,
                id_field,
            } => (
                format!("{entity_type}[{id_field}].status"),
                env.cross_statuses(entity_type, id_field)
                    .unwrap_or_default()
                    .into_iter()
                    .map(render)
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        };
        if !found.iter().any(|(existing, _)| *existing == label) {
            found.push((label, value));
        }
    });
    Some(GuardFailure {
        expr: failing.to_string(),
        found,
    })
}

fn render(value: Val<'_>) -> String {
    match value {
        Val::Unknown => "<unresolved>".into(),
        Val::Null => "<missing>".into(),
        Val::Bool(b) => b.to_string(),
        Val::Num(n) => n.to_string(),
        Val::Str(s) => s.to_string(),
        Val::Strs(items) => format!("[{}]", items.join(", ")),
        Val::Json(json) => json.to_string(),
    }
}

#[cfg(test)]
#[path = "guard_test.rs"]
mod tests;
