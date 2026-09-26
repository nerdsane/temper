//! Three-valued evaluation of predicate expressions.
//!
//! At runtime every value is known and evaluation is ordinary boolean logic.
//! The verifier cannot know some values (a related entity's status, a field
//! it does not model); those read as [`Val::Unknown`] and propagate with
//! Kleene logic, so "may hold" is `!= False` and "must hold" is `== True`
//! under any combination of `!`, `&&`, `||` and `=>`.

use super::ast::{CmpOp, Expr, Literal, Operand, Set};

/// A three-valued truth value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truth {
    /// Definitely holds.
    True,
    /// Definitely does not hold.
    False,
    /// Depends on a value the evaluator cannot see.
    Unknown,
}

impl Truth {
    fn from_bool(value: bool) -> Self {
        if value { Truth::True } else { Truth::False }
    }

    fn not(self) -> Self {
        match self {
            Truth::True => Truth::False,
            Truth::False => Truth::True,
            Truth::Unknown => Truth::Unknown,
        }
    }

    /// `true` unless the predicate definitely fails.
    pub fn may_hold(self) -> bool {
        self != Truth::False
    }

    /// `true` only if the predicate definitely holds.
    pub fn must_hold(self) -> bool {
        self == Truth::True
    }
}

/// A value read from the evaluation environment.
#[derive(Debug, Clone, Copy)]
pub enum Val<'a> {
    /// The evaluator cannot know this value.
    Unknown,
    /// Absent or `null`.
    Null,
    /// A boolean.
    Bool(bool),
    /// A number.
    Num(f64),
    /// A string.
    Str(&'a str),
    /// A list of strings (IOA list/set state variables).
    Strs(&'a [String]),
    /// A JSON array or object from an entity field.
    Json(&'a serde_json::Value),
}

impl<'a> Val<'a> {
    /// Borrow a JSON value, normalising scalars.
    pub fn json(value: &'a serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Val::Null,
            serde_json::Value::Bool(b) => Val::Bool(*b),
            serde_json::Value::Number(n) => n.as_f64().map_or(Val::Null, Val::Num),
            serde_json::Value::String(s) => Val::Str(s),
            other => Val::Json(other),
        }
    }

    fn list_len(self) -> Option<usize> {
        match self {
            Val::Strs(items) => Some(items.len()),
            Val::Json(serde_json::Value::Array(items)) => Some(items.len()),
            _ => None,
        }
    }
}

/// Where an expression reads its values from.
pub trait Env {
    /// The entity's status.
    fn status(&self) -> Val<'_>;
    /// A state variable or field. Absent names read as [`Val::Null`].
    fn var(&self, name: &str) -> Val<'_>;
    /// The statuses of the related entities of `entity_type` whose ids are
    /// in `id_field` (one id, or a list of ids). An unset reference, or a
    /// related entity that cannot be found, contributes one `Val::Null`, so
    /// the result is never empty. `None` when the evaluator cannot know.
    fn cross_statuses(&self, entity_type: &str, id_field: &str) -> Option<Vec<Val<'_>>>;
}

/// Evaluate `expr` against `env`.
pub fn eval(expr: &Expr, env: &dyn Env) -> Truth {
    match expr {
        Expr::Const(value) => Truth::from_bool(*value),
        Expr::Not(inner) => eval(inner, env).not(),
        Expr::And(parts) => {
            let mut result = Truth::True;
            for part in parts {
                match eval(part, env) {
                    Truth::False => return Truth::False,
                    Truth::Unknown => result = Truth::Unknown,
                    Truth::True => {}
                }
            }
            result
        }
        Expr::Or(parts) => {
            let mut result = Truth::False;
            for part in parts {
                match eval(part, env) {
                    Truth::True => return Truth::True,
                    Truth::Unknown => result = Truth::Unknown,
                    Truth::False => {}
                }
            }
            result
        }
        Expr::Implies(lhs, rhs) => match eval(lhs, env) {
            Truth::False => Truth::True,
            Truth::True => eval(rhs, env),
            Truth::Unknown => match eval(rhs, env) {
                Truth::True => Truth::True,
                _ => Truth::Unknown,
            },
        },
        Expr::Compare { lhs, op, rhs } => for_each_value(lhs, env, |a| {
            for_each_value(rhs, env, |b| compare(a, *op, b))
        }),
        Expr::In {
            value,
            set,
            negated,
        } => for_each_value(value, env, |value| {
            let result = contains(env, set, value);
            if *negated { result.not() } else { result }
        }),
        Expr::Empty(name) => match env.var(name) {
            Val::Unknown => Truth::Unknown,
            Val::Null => Truth::True,
            Val::Str(s) => Truth::from_bool(s.is_empty()),
            other => Truth::from_bool(other.list_len() == Some(0)),
        },
        Expr::Var(name) => match env.var(name) {
            Val::Unknown => Truth::Unknown,
            Val::Bool(b) => Truth::from_bool(b),
            _ => Truth::False,
        },
    }
}

/// Evaluate `test` for the value of `operand`. A related-entity status
/// yields one value per related entity and must pass for all of them.
fn for_each_value(operand: &Operand, env: &dyn Env, test: impl Fn(Val<'_>) -> Truth) -> Truth {
    let value = match operand {
        Operand::Status => env.status(),
        Operand::Var(name) => env.var(name),
        Operand::Len(name) => match env.var(name) {
            Val::Unknown => Val::Unknown,
            other => other
                .list_len()
                .map_or(Val::Null, |len| Val::Num(len as f64)),
        },
        Operand::Lit(lit) => literal(lit),
        Operand::CrossStatus {
            entity_type,
            id_field,
        } => {
            let Some(statuses) = env.cross_statuses(entity_type, id_field) else {
                return Truth::Unknown;
            };
            debug_assert!(!statuses.is_empty(), "cross_statuses is never empty");
            let mut result = Truth::True;
            for status in statuses {
                match test(status) {
                    Truth::False => return Truth::False,
                    Truth::Unknown => result = Truth::Unknown,
                    Truth::True => {}
                }
            }
            return result;
        }
    };
    test(value)
}

fn literal(lit: &Literal) -> Val<'_> {
    match lit {
        Literal::Int(value) => Val::Num(*value as f64),
        Literal::Str(value) => Val::Str(value),
        Literal::Bool(value) => Val::Bool(*value),
        Literal::Null => Val::Null,
    }
}

fn compare(lhs: Val<'_>, op: CmpOp, rhs: Val<'_>) -> Truth {
    if matches!(lhs, Val::Unknown) || matches!(rhs, Val::Unknown) {
        return Truth::Unknown;
    }
    let result = match op {
        CmpOp::Eq => equal(lhs, rhs),
        CmpOp::Ne => !equal(lhs, rhs),
        CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge => match (lhs, rhs) {
            (Val::Num(a), Val::Num(b)) => match op {
                CmpOp::Lt => a < b,
                CmpOp::Le => a <= b,
                CmpOp::Gt => a > b,
                _ => a >= b,
            },
            _ => false,
        },
    };
    Truth::from_bool(result)
}

fn equal(lhs: Val<'_>, rhs: Val<'_>) -> bool {
    match (lhs, rhs) {
        (Val::Null, Val::Null) => true,
        (Val::Bool(a), Val::Bool(b)) => a == b,
        (Val::Num(a), Val::Num(b)) => a == b,
        (Val::Str(a), Val::Str(b)) => a == b,
        (Val::Strs(a), Val::Strs(b)) => a == b,
        (Val::Json(a), Val::Json(b)) => a == b,
        _ => false,
    }
}

fn contains(env: &dyn Env, set: &Set, value: Val<'_>) -> Truth {
    if matches!(value, Val::Unknown) {
        return Truth::Unknown;
    }
    match set {
        Set::List(items) => Truth::from_bool(items.iter().any(|item| equal(value, literal(item)))),
        Set::Var(name) => match env.var(name) {
            Val::Unknown => Truth::Unknown,
            Val::Strs(items) => {
                Truth::from_bool(matches!(value, Val::Str(s) if items.iter().any(|item| item == s)))
            }
            Val::Json(serde_json::Value::Array(items)) => {
                Truth::from_bool(items.iter().any(|item| equal(value, Val::json(item))))
            }
            _ => Truth::False,
        },
    }
}

/// The smallest sub-expression responsible for `expr` not holding, for
/// error messages. Descends through `&&`; any other failing node is reported
/// whole. Returns `None` when `expr` does not definitely fail.
pub fn explain<'e>(expr: &'e Expr, env: &dyn Env) -> Option<&'e Expr> {
    if eval(expr, env) != Truth::False {
        return None;
    }
    match expr {
        Expr::And(parts) => parts.iter().find_map(|part| explain(part, env)),
        other => Some(other),
    }
}

/// An environment over a JSON object of entity fields. `status` reads the
/// `Status` field; related-entity statuses are unknown.
#[derive(Debug, Clone, Copy)]
pub struct JsonEnv<'a> {
    fields: &'a serde_json::Value,
}

impl<'a> JsonEnv<'a> {
    /// Wrap a fields object.
    pub fn new(fields: &'a serde_json::Value) -> Self {
        Self { fields }
    }
}

impl Env for JsonEnv<'_> {
    fn status(&self) -> Val<'_> {
        self.var("Status")
    }
    fn var(&self, name: &str) -> Val<'_> {
        self.fields.get(name).map_or(Val::Null, Val::json)
    }
    fn cross_statuses(&self, _: &str, _: &str) -> Option<Vec<Val<'_>>> {
        None
    }
}
