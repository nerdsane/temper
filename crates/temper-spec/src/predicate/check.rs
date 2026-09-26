//! Name resolution and type checking.

use std::collections::BTreeMap;

use super::ast::{CmpOp, Expr, Literal, Operand, Set};

/// The type of a declared state variable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VarKind {
    /// `counter`: a non-negative integer the verifier models.
    Counter,
    /// `bool`.
    Bool,
    /// `list` or `set` of strings.
    List,
    /// `string` or `status`.
    Str,
    /// `int`, `integer`, `float` or `number`: numeric but not modeled.
    Num,
}

impl VarKind {
    /// Map an IOA `[[state]] type` to its kind. Unknown types read as `Str`.
    pub fn from_type(var_type: &str) -> Self {
        match var_type {
            "counter" => VarKind::Counter,
            "bool" => VarKind::Bool,
            "list" | "set" => VarKind::List,
            "int" | "integer" | "float" | "number" => VarKind::Num,
            _ => VarKind::Str,
        }
    }
}

/// Which names an expression may read.
#[derive(Debug, Clone, Copy)]
pub enum Scope<'a> {
    /// `status` and the declared state variables (action guards, invariants).
    /// Reference fields (`Type[id].status`, `empty(id)`) may be any field.
    State(&'a BTreeMap<String, VarKind>),
    /// `status` and any entity field (trigger guards, field invariants).
    Fields,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Num,
    Str,
    Bool,
    List,
    Null,
    /// A field of unknown type (field scope).
    Any,
}

/// Check that every name exists and every comparison is well typed.
pub fn check(expr: &Expr, scope: Scope<'_>) -> Result<(), String> {
    match expr {
        Expr::Const(_) => Ok(()),
        Expr::Not(inner) => check(inner, scope),
        Expr::And(parts) | Expr::Or(parts) => parts.iter().try_for_each(|p| check(p, scope)),
        Expr::Implies(a, b) => {
            check(a, scope)?;
            check(b, scope)
        }
        Expr::Var(name) => match var_ty(name, scope)? {
            Ty::Bool | Ty::Any => Ok(()),
            _ => Err(format!("'{name}' is not a bool; compare it explicitly")),
        },
        Expr::Empty(name) => match field_ty(name, scope) {
            Ty::Str | Ty::List | Ty::Any => Ok(()),
            _ => Err(format!(
                "empty() needs a string or list, '{name}' is neither"
            )),
        },
        Expr::Compare { lhs, op, rhs } => {
            let (a, b) = (operand_ty(lhs, scope)?, operand_ty(rhs, scope)?);
            let ordering = !matches!(op, CmpOp::Eq | CmpOp::Ne);
            if ordering && !(numeric(a) && numeric(b)) {
                return Err(format!("'{}' compares non-numbers", expr));
            }
            if compatible(a, b) {
                Ok(())
            } else {
                Err(format!("'{expr}' compares values of different types"))
            }
        }
        Expr::In { value, set, .. } => {
            let value_ty = operand_ty(value, scope)?;
            match set {
                Set::List(items) => {
                    for item in items {
                        if !compatible(value_ty, lit_ty(item)) {
                            return Err(format!("'{expr}' mixes types in its list"));
                        }
                    }
                    Ok(())
                }
                Set::Var(name) => match var_ty(name, scope)? {
                    Ty::List | Ty::Any if compatible(value_ty, Ty::Str) => Ok(()),
                    Ty::List | Ty::Any => Err(format!("'{expr}': lists hold strings")),
                    _ => Err(format!("'{name}' is not a list")),
                },
            }
        }
    }
}

/// The first sub-expression the verification cascade cannot model, if any.
/// `[[invariant]]` asserts must return `None`.
pub fn unmodelable<'e>(expr: &'e Expr, vars: &BTreeMap<String, VarKind>) -> Option<&'e Expr> {
    let kind = |name: &str| vars.get(name).copied();
    let modeled_operand = |operand: &Operand| match operand {
        Operand::Status | Operand::Lit(Literal::Int(_) | Literal::Str(_) | Literal::Bool(_)) => {
            true
        }
        Operand::Var(name) => matches!(kind(name), Some(VarKind::Counter | VarKind::Bool)),
        Operand::Len(name) => kind(name) == Some(VarKind::List),
        Operand::CrossStatus { .. } | Operand::Lit(Literal::Null) => false,
    };
    match expr {
        Expr::Const(_) => None,
        Expr::Not(inner) => unmodelable(inner, vars),
        Expr::And(parts) | Expr::Or(parts) => parts.iter().find_map(|p| unmodelable(p, vars)),
        Expr::Implies(a, b) => unmodelable(a, vars).or_else(|| unmodelable(b, vars)),
        Expr::Var(name) => (kind(name) != Some(VarKind::Bool)).then_some(expr),
        Expr::Empty(name) => (kind(name) != Some(VarKind::List)).then_some(expr),
        Expr::Compare { lhs, rhs, .. } => {
            (!modeled_operand(lhs) || !modeled_operand(rhs)).then_some(expr)
        }
        Expr::In { value, set, .. } => {
            let modeled = match set {
                Set::List(_) => modeled_operand(value),
                Set::Var(name) => {
                    kind(name) == Some(VarKind::List)
                        && matches!(value, Operand::Lit(Literal::Str(_)))
                }
            };
            (!modeled).then_some(expr)
        }
    }
}

fn var_ty(name: &str, scope: Scope<'_>) -> Result<Ty, String> {
    match scope {
        Scope::Fields => Ok(Ty::Any),
        Scope::State(vars) => match vars.get(name) {
            Some(VarKind::Counter | VarKind::Num) => Ok(Ty::Num),
            Some(VarKind::Bool) => Ok(Ty::Bool),
            Some(VarKind::List) => Ok(Ty::List),
            Some(VarKind::Str) => Ok(Ty::Str),
            None => Err(format!("unknown state variable '{name}'")),
        },
    }
}

/// Like [`var_ty`], but an undeclared name is an entity field of unknown
/// type. Used where a spec names a reference field (`Type[id].status`,
/// `empty(id)`), which is often a data-model property, not a state variable.
fn field_ty(name: &str, scope: Scope<'_>) -> Ty {
    var_ty(name, scope).unwrap_or(Ty::Any)
}

fn operand_ty(operand: &Operand, scope: Scope<'_>) -> Result<Ty, String> {
    match operand {
        Operand::Status => Ok(Ty::Str),
        Operand::Var(name) => var_ty(name, scope),
        Operand::Len(name) => match var_ty(name, scope)? {
            Ty::List | Ty::Any => Ok(Ty::Num),
            _ => Err(format!("len() needs a list, '{name}' is not one")),
        },
        Operand::CrossStatus { id_field, .. } => match field_ty(id_field, scope) {
            Ty::Str | Ty::List | Ty::Any => Ok(Ty::Str),
            _ => Err(format!("'{id_field}' cannot hold an entity id")),
        },
        Operand::Lit(lit) => Ok(lit_ty(lit)),
    }
}

fn lit_ty(lit: &Literal) -> Ty {
    match lit {
        Literal::Int(_) => Ty::Num,
        Literal::Str(_) => Ty::Str,
        Literal::Bool(_) => Ty::Bool,
        Literal::Null => Ty::Null,
    }
}

fn numeric(ty: Ty) -> bool {
    matches!(ty, Ty::Num | Ty::Any)
}

fn compatible(a: Ty, b: Ty) -> bool {
    a == b || matches!(a, Ty::Any) || matches!(b, Ty::Any)
}
