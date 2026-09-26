//! Lower the pre-grammar predicate forms into [`Expr`].
//!
//! Each function preserves the old evaluation semantics exactly; the
//! differential tests in `lower_test.rs` compare both evaluators on
//! generated states.

use super::assert_parser::{AssertCompareOp, ParsedAssert, parse_assert_expr};
use super::field_predicate::FieldPredicate;
use super::syntax::{Guard, TriggerGuard};
use crate::predicate::{CmpOp, Expr, Literal, Operand, Set};

/// Result of lowering one `[[invariant]]`.
#[derive(Debug, Clone, PartialEq)]
pub enum LoweredInvariant {
    /// An ordinary assertion.
    Assert(Expr),
    /// `no_further_transitions`: the `when` states are terminal.
    Terminal(Vec<String>),
    /// A history predicate (`ordering(A, B)`), no longer supported.
    Dropped(String),
}

fn var(name: &str) -> Operand {
    Operand::Var(name.to_string())
}

fn int(value: usize) -> Operand {
    Operand::Lit(Literal::Int(value as i64))
}

fn strs(values: &[String]) -> Set {
    Set::List(values.iter().map(|v| Literal::Str(v.clone())).collect())
}

fn cmp(lhs: Operand, op: CmpOp, rhs: Operand) -> Expr {
    Expr::Compare { lhs, op, rhs }
}

/// `status in [...]`.
pub fn status_in(states: &[String]) -> Expr {
    Expr::In {
        value: Operand::Status,
        set: strs(states),
        negated: false,
    }
}

/// Lower an action's guard list (implicitly AND-ed).
pub fn guards_to_expr(guards: &[Guard]) -> Expr {
    Expr::and(guards.iter().map(guard_to_expr).collect())
}

/// Lower one action guard.
pub fn guard_to_expr(guard: &Guard) -> Expr {
    match guard {
        Guard::StateIn { values } => status_in(values),
        Guard::MinCount { var: name, min } => cmp(var(name), CmpOp::Ge, int(*min)),
        Guard::MaxCount { var: name, max } => cmp(var(name), CmpOp::Lt, int(*max)),
        Guard::IsTrue { var: name } => Expr::Var(name.clone()),
        Guard::IsFalse { var: name } => Expr::Not(Box::new(Expr::Var(name.clone()))),
        Guard::ListContains { var: name, value } => Expr::In {
            value: Operand::Lit(Literal::Str(value.clone())),
            set: Set::Var(name.clone()),
            negated: false,
        },
        Guard::ListLengthMin { var: name, min } => {
            cmp(Operand::Len(name.clone()), CmpOp::Ge, int(*min))
        }
        Guard::CrossEntityState {
            entity_type,
            entity_id_source,
            required_status,
            forbidden_status,
            required,
        } => cross_entity(
            entity_type,
            entity_id_source,
            required_status,
            forbidden_status,
            *required,
        ),
    }
}

/// The old `cross_entity_state` flags, as an expression. An unset
/// reference or an unresolved related entity reads as `null`:
/// - allowlist: `status in R` (null fails)
/// - denylist: `status not in F` (null passes)
/// - `required`: null fails, so add `!= null` when there is no allowlist
/// - not `required` with an allowlist: an unset reference passes, so wrap in
///   `empty(ref) || ...` (an unresolved one still fails, as before)
fn cross_entity(
    entity_type: &str,
    id_field: &str,
    required_status: &[String],
    forbidden_status: &[String],
    required: bool,
) -> Expr {
    let status = || Operand::CrossStatus {
        entity_type: entity_type.to_string(),
        id_field: id_field.to_string(),
    };
    let mut parts = Vec::new();
    if !required_status.is_empty() {
        parts.push(Expr::In {
            value: status(),
            set: strs(required_status),
            negated: false,
        });
    } else if required {
        parts.push(cmp(status(), CmpOp::Ne, Operand::Lit(Literal::Null)));
    }
    if !forbidden_status.is_empty() {
        parts.push(Expr::In {
            value: status(),
            set: strs(forbidden_status),
            negated: true,
        });
    }
    let checked = Expr::and(parts);
    if !required && !required_status.is_empty() {
        Expr::Or(vec![Expr::Empty(id_field.to_string()), checked])
    } else {
        checked
    }
}

/// Lower an `[[invariant]]`'s `when` and `assert`.
pub fn invariant_to_expr(when: &[String], assert: &str) -> Result<LoweredInvariant, String> {
    let body = match parse_assert_expr(assert) {
        Some(ParsedAssert::NoFurtherTransitions) => {
            return Ok(LoweredInvariant::Terminal(when.to_vec()));
        }
        Some(ParsedAssert::OrderingConstraint { .. }) => {
            return Ok(LoweredInvariant::Dropped(assert.to_string()));
        }
        Some(parsed) => parsed_assert_to_expr(&parsed)?,
        // Shapes the old assert parser never understood: guard syntax such as
        // `is_true x`, or comparisons it rejected such as `x != ''`.
        None => match super::guard_syntax::parse_guard_clause(assert) {
            Ok(guard) => guard_to_expr(&guard),
            Err(_) => crate::predicate::parse(assert).map_err(|e| e.to_string())?,
        },
    };
    Ok(LoweredInvariant::Assert(if when.is_empty() {
        body
    } else {
        Expr::Implies(Box::new(status_in(when)), Box::new(body))
    }))
}

fn parsed_assert_to_expr(parsed: &ParsedAssert) -> Result<Expr, String> {
    Ok(match parsed {
        ParsedAssert::CounterPositive { var: name } => cmp(var(name), CmpOp::Gt, int(0)),
        ParsedAssert::CounterCompare {
            var: name,
            op,
            value,
        } => cmp(
            var(name),
            match op {
                AssertCompareOp::Gt => CmpOp::Gt,
                AssertCompareOp::Gte => CmpOp::Ge,
                AssertCompareOp::Lt => CmpOp::Lt,
                AssertCompareOp::Lte => CmpOp::Le,
                AssertCompareOp::Eq => CmpOp::Eq,
            },
            int(*value),
        ),
        ParsedAssert::BoolRequired { var: name, expect } => {
            let atom = match name.as_str() {
                "true" => Expr::Const(true),
                "false" => Expr::Const(false),
                _ => Expr::Var(name.clone()),
            };
            if *expect {
                atom
            } else {
                Expr::Not(Box::new(atom))
            }
        }
        ParsedAssert::NeverState { state } => cmp(
            Operand::Status,
            CmpOp::Ne,
            Operand::Lit(Literal::Str(state.clone())),
        ),
        ParsedAssert::And(parts) => Expr::And(
            parts
                .iter()
                .map(parsed_assert_to_expr)
                .collect::<Result<_, _>>()?,
        ),
        ParsedAssert::Or(parts) => Expr::Or(
            parts
                .iter()
                .map(parsed_assert_to_expr)
                .collect::<Result<_, _>>()?,
        ),
        ParsedAssert::NoFurtherTransitions | ParsedAssert::OrderingConstraint { .. } => {
            return Err("no_further_transitions and ordering() cannot be combined".into());
        }
    })
}

/// Lower an `[[action.triggers]]` guard.
pub fn trigger_guard_to_expr(guard: &TriggerGuard) -> Result<Expr, String> {
    Ok(match guard {
        TriggerGuard::FieldEquals { field, value } => {
            cmp(var(field), CmpOp::Eq, Operand::Lit(json_literal(value)?))
        }
        TriggerGuard::FieldIn { field, values } => Expr::In {
            value: var(field),
            set: Set::List(values.iter().map(json_literal).collect::<Result<_, _>>()?),
            negated: false,
        },
        TriggerGuard::BoolTrue { field } => Expr::Var(field.clone()),
        // Old semantics: only an explicit `false` passes; missing fails.
        TriggerGuard::BoolFalse { field } => {
            cmp(var(field), CmpOp::Eq, Operand::Lit(Literal::Bool(false)))
        }
        TriggerGuard::StateIn { values } => status_in(values),
        TriggerGuard::CrossEntityStateIn {
            entity_type,
            entity_id_source,
            required_status,
        } => Expr::In {
            value: Operand::CrossStatus {
                entity_type: entity_type.clone(),
                id_field: entity_id_source.clone(),
            },
            set: strs(required_status),
            negated: false,
        },
        TriggerGuard::AllOf { guards } => Expr::And(
            guards
                .iter()
                .map(trigger_guard_to_expr)
                .collect::<Result<_, _>>()?,
        ),
        TriggerGuard::AnyOf { guards } => Expr::Or(
            guards
                .iter()
                .map(trigger_guard_to_expr)
                .collect::<Result<_, _>>()?,
        ),
        TriggerGuard::Not { guard } => Expr::Not(Box::new(trigger_guard_to_expr(guard)?)),
    })
}

/// Lower a `[[field_invariant]]`'s `when` and `require`.
pub fn field_invariant_to_expr(
    when: &FieldPredicate,
    require: &FieldPredicate,
) -> Result<Expr, String> {
    Ok(Expr::Implies(
        Box::new(field_predicate_to_expr(when)?),
        Box::new(field_predicate_to_expr(require)?),
    ))
}

/// Lower one field predicate.
pub fn field_predicate_to_expr(predicate: &FieldPredicate) -> Result<Expr, String> {
    Ok(match predicate {
        FieldPredicate::Absent { field, .. } => {
            cmp(var(field), CmpOp::Eq, Operand::Lit(Literal::Null))
        }
        FieldPredicate::Equals { field, equals } => {
            cmp(var(field), CmpOp::Eq, Operand::Lit(json_literal(equals)?))
        }
        FieldPredicate::Empty { field, .. } => Expr::Empty(field.clone()),
        FieldPredicate::AnyOf { any_of } => Expr::Or(
            any_of
                .iter()
                .map(field_predicate_to_expr)
                .collect::<Result<_, _>>()?,
        ),
        FieldPredicate::AllOf { all_of } => Expr::And(
            all_of
                .iter()
                .map(field_predicate_to_expr)
                .collect::<Result<_, _>>()?,
        ),
        FieldPredicate::Not { not } => Expr::Not(Box::new(field_predicate_to_expr(not)?)),
    })
}

fn json_literal(value: &serde_json::Value) -> Result<Literal, String> {
    match value {
        serde_json::Value::Null => Ok(Literal::Null),
        serde_json::Value::Bool(b) => Ok(Literal::Bool(*b)),
        serde_json::Value::String(s) if !s.contains('\'') => Ok(Literal::Str(s.clone())),
        serde_json::Value::Number(n) => n
            .as_i64()
            .map(Literal::Int)
            .ok_or_else(|| format!("non-integer literal {n} is not supported")),
        other => Err(format!("literal {other} is not supported")),
    }
}
