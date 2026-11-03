//! Guard evaluation.
//!
//! A guard is a [`temper_spec::predicate::Expr`] on a
//! [`super::types::TransitionRule`], evaluated against an [`EvalContext`]
//! before the transition fires. [`check`] is the hot path; [`check_detailed`]
//! runs only on rejection and names the failing sub-expression so the server
//! can render a self-heal error (ADR-0151).

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use temper_spec::predicate::{
    CmpOp, Env, Expr, Literal, Name, Operand, Set, Truth, Val, eval, explain,
};

use super::action_contract::InitialValues;
use super::refusal::GuardCondition;

/// The failing part of a rejected guard (ADR-0151).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuardFailure {
    /// The smallest failing sub-expression, in canonical form.
    pub expr: String,
    /// Each name that sub-expression reads, with the value found.
    pub found: Vec<(String, String)>,
    /// Each comparison in that sub-expression that splits into a field, an
    /// operator and a wanted value, with the typed value read.
    pub conditions: Vec<GuardCondition>,
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
    /// The parameters of the move being taken, which its guard reads as
    /// `params.p`. An absent parameter reads as `null`.
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// Related entities' statuses keyed by `(entity_type, id_field)`.
pub type RelatedMap = BTreeMap<(String, String), Related>;

/// The statuses of the entities one reference field points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Related {
    /// One status per id; `None` for an id whose entity was not found. Empty
    /// when the reference is unset.
    Statuses(Vec<Option<String>>),
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
        let (ctx, declared) = (self.ctx, self.declared);
        if let Some(value) = declared.counter(&ctx.counters, name) {
            return Val::Num(value as f64);
        }
        if let Some(value) = declared.boolean(&ctx.booleans, name) {
            return Val::Bool(value);
        }
        if let Some(value) = declared.list(&ctx.lists, name) {
            return Val::Strs(value);
        }
        if let Some(value) = declared.field(ctx.fields.get(name), name) {
            return Val::json(value);
        }
        Val::Null
    }

    fn cross_statuses(&self, entity_type: &str, id_field: &str) -> Option<Vec<Val<'_>>> {
        let key = (entity_type.to_string(), id_field.to_string());
        match self.ctx.related.get(&key) {
            Some(Related::Statuses(statuses)) if !statuses.is_empty() => Some(
                statuses
                    .iter()
                    .map(|status| status.as_deref().map_or(Val::Null, Val::Str))
                    .collect(),
            ),
            _ => Some(vec![Val::Null]),
        }
    }

    fn param(&self, name: &str) -> Val<'_> {
        self.ctx.params.get(name).map_or(Val::Null, Val::json)
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
    // A guard holds only when it is true; unknown never passes.
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
            Name::Param(param) => (format!("params.{param}"), render(env.param(param))),
            // Refused in guards at load (validate_predicates).
            Name::CrossField {
                entity_type,
                id_field,
                field,
            } => (
                format!("{entity_type}[{id_field}].{field}"),
                "?".to_string(),
            ),
        };
        if !found.iter().any(|(existing, _)| *existing == label) {
            found.push((label, value));
        }
    });
    let mut conditions = Vec::new();
    split_conditions(failing, &env, &mut conditions);
    Some(GuardFailure {
        expr: failing.to_string(),
        found,
        conditions,
    })
}

/// Push one [`GuardCondition`] per comparison of `expr` between a field and a
/// literal, descending through `&&`. Anything else (`||`, `=>`, a call such as
/// `len` or `empty`, a comparison of two fields, a cross-entity status) has no
/// single field and wanted value, so it adds nothing.
fn split_conditions(expr: &Expr, env: &GuardEnv<'_>, out: &mut Vec<GuardCondition>) {
    let mut push = |field: &str, op: &str, wants: serde_json::Value, value: serde_json::Value| {
        out.push(GuardCondition {
            field: field.to_string(),
            op: op.to_string(),
            wants,
            value,
        });
    };
    match expr {
        Expr::And(parts) => {
            for part in parts {
                split_conditions(part, env, out);
            }
        }
        Expr::Var(name) => push(name, "==", true.into(), condition_var(name, env)),
        Expr::Not(inner) => {
            if let Expr::Var(name) = inner.as_ref() {
                push(name, "==", false.into(), condition_var(name, env));
            }
        }
        Expr::Compare { lhs, op, rhs } => match (lhs, rhs) {
            (Operand::Lit(lit), operand) => {
                if let Some((field, value)) = field_operand(operand, env) {
                    push(field, flip(*op).as_str(), literal_json(lit), value);
                }
            }
            (operand, Operand::Lit(lit)) => {
                if let Some((field, value)) = field_operand(operand, env) {
                    push(field, op.as_str(), literal_json(lit), value);
                }
            }
            _ => {}
        },
        Expr::In {
            value,
            set: Set::List(items),
            negated,
        } => {
            if let Some((field, read)) = field_operand(value, env) {
                let op = if *negated { "not in" } else { "in" };
                let wants = items.iter().map(literal_json).collect();
                push(field, op, serde_json::Value::Array(wants), read);
            }
        }
        _ => {}
    }
}

/// The name and value of an operand that reads one field of this entity.
fn field_operand<'e>(
    operand: &'e Operand,
    env: &'e GuardEnv<'_>,
) -> Option<(&'e str, serde_json::Value)> {
    match operand {
        Operand::Status => Some(("status", env.status.into())),
        Operand::Var(name) => Some((name, condition_var(name, env))),
        _ => None,
    }
}

/// Read stored values without the evaluator's floating-point conversion.
fn condition_var(name: &str, env: &GuardEnv<'_>) -> serde_json::Value {
    let (ctx, declared) = (env.ctx, env.declared);
    if let Some(value) = declared.counter(&ctx.counters, name) {
        return value.into();
    }
    if let Some(value) = declared.boolean(&ctx.booleans, name) {
        return value.into();
    }
    if let Some(value) = declared.list(&ctx.lists, name) {
        return value.to_vec().into();
    }
    if let Some(value) = declared.field(ctx.fields.get(name), name) {
        return value.clone();
    }
    to_json(env.var(name))
}

/// `lit op x` read as `x op' lit`.
fn flip(op: CmpOp) -> CmpOp {
    match op {
        CmpOp::Lt => CmpOp::Gt,
        CmpOp::Le => CmpOp::Ge,
        CmpOp::Gt => CmpOp::Lt,
        CmpOp::Ge => CmpOp::Le,
        same => same,
    }
}

fn literal_json(lit: &Literal) -> serde_json::Value {
    match lit {
        Literal::Int(value) => (*value).into(),
        Literal::Str(value) => value.as_str().into(),
        Literal::Bool(value) => (*value).into(),
        Literal::Null => serde_json::Value::Null,
    }
}

/// A read value as JSON. Counters are whole numbers and answer as integers.
fn to_json(value: Val<'_>) -> serde_json::Value {
    match value {
        Val::Unknown | Val::Null => serde_json::Value::Null,
        Val::Bool(b) => b.into(),
        Val::Num(n) if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 => (n as i64).into(),
        Val::Num(n) => serde_json::Number::from_f64(n).map_or(serde_json::Value::Null, Into::into),
        Val::Str(s) => s.into(),
        Val::Symbol(n) => serde_json::json!({ "symbol": n }),
        Val::Strs(items) => items.to_vec().into(),
        Val::Values(items) => items.into_iter().map(to_json).collect(),
        Val::Json(json) => json.clone(),
    }
}

fn render(value: Val<'_>) -> String {
    match value {
        Val::Unknown => "<unresolved>".into(),
        Val::Null => "<missing>".into(),
        Val::Bool(b) => b.to_string(),
        Val::Num(n) => n.to_string(),
        Val::Str(s) => s.to_string(),
        Val::Symbol(n) => format!("<symbol:{n}>"),
        Val::Strs(items) => format!("[{}]", items.join(", ")),
        Val::Values(items) => format!(
            "[{}]",
            items.into_iter().map(render).collect::<Vec<_>>().join(", ")
        ),
        Val::Json(json) => json.to_string(),
    }
}

#[cfg(test)]
#[path = "guard_test.rs"]
mod tests;
