//! A trigger's guard reads its source move's parameters by bare name.
//!
//! The runtime copies every parameter of a committed move into the record's
//! fields (`sync_fields_with_metadata` in the server), and a trigger's guard
//! reads those fields, so a bare name that is a parameter of the move reads
//! the value the move carried. Counters, booleans and lists are written over
//! the copy from their own storage, so a variable of one of those kinds
//! wins; an `int` lives in the fields, so the copy is its value. The model
//! reads such a name as `params.name`, and gives the move a value for it.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Action, Automaton, VarType};
use temper_spec::predicate::{CmpOp, Expr, Literal, Operand, ParamKind, Truth, eval};

use super::count_bounds::count_ranges;
use super::expansion::Expansion;
use super::fields::Fields;
use super::heap::Heap;
use super::parameters::{MoveEnv, guard_count_bound};
use super::semantics::ModelEnv;
use super::types::{ParamValue, ResolvedTransition, TemperModel, TemperModelState};

/// `guard` as the model reads it after `action` commits on `automaton`:
/// each bare name the runtime's copy of `action`'s parameters gives its
/// value read as that parameter.
pub(crate) fn as_read(automaton: &Automaton, action: &Action, guard: &Expr) -> Expr {
    let copied = |name: &str| copied(automaton, action, name);
    rewrite(guard, &copied)
}

/// What follows `action` reads of the values it carried: its triggers'
/// guards as the model reads them, each guard one part, so a part names
/// only what its own guard compares.
pub(crate) fn after(automaton: &Automaton, action: &Action) -> Expr {
    let parts: Vec<Expr> = action
        .triggers
        .iter()
        .filter_map(|t| t.guard.as_ref())
        .map(|guard| as_read(automaton, action, guard))
        .collect();
    match parts.len() {
        0 => Expr::always(),
        1 => parts.into_iter().next().expect("one part"),
        _ => Expr::And(parts),
    }
}

/// The parameters of `action` its trigger guards read by bare name, with
/// the kind of value each takes: its declared type, or for a bare name as
/// the guards use it (a condition, or a number compared).
pub(crate) fn params_read(automaton: &Automaton, action: &Action) -> BTreeMap<String, ParamKind> {
    let mut kinds = BTreeMap::new();
    for guard in action.triggers.iter().filter_map(|t| t.guard.as_ref()) {
        uses(guard, &mut |name, used| {
            if !copied(automaton, action, name) {
                return;
            }
            let declared = action
                .params
                .iter()
                .find(|p| p.name() == name)
                .filter(|p| !matches!(p, temper_spec::automaton::ActionParam::Named(_)))
                .map(|p| p.param_type());
            let kind = match declared {
                Some(VarType::Bool) => Some(ParamKind::Bool),
                Some(VarType::Counter) => Some(ParamKind::Count),
                Some(_) => None,
                None => used,
            };
            if let Some(kind) = kind {
                kinds.entry(name.to_string()).or_insert(kind);
            }
        });
    }
    kinds
}

/// Each move in `made` once for each way the values only its triggers
/// read decide their guards: those values change nothing the move writes,
/// so the record it leaves stays as it is.
pub(crate) fn with_after_values(
    model: &TemperModel,
    t: &ResolvedTransition,
    made: Vec<(Fields<ParamValue>, TemperModelState)>,
    budget: &Expansion,
) -> Vec<(Fields<ParamValue>, TemperModelState)> {
    let ranges = count_ranges(model, t);
    let domains: Vec<(&String, Vec<ParamValue>)> = t
        .after_params
        .iter()
        .map(|(name, kind)| {
            let values = match kind {
                ParamKind::Bool => vec![ParamValue::Bool(false), ParamValue::Bool(true)],
                ParamKind::Count => (0..=guard_count_bound(model, &t.after, name, &ranges))
                    .map(ParamValue::Count)
                    .collect(),
                ParamKind::Str => vec![ParamValue::Fresh],
            };
            (name, values)
        })
        .collect();
    let mut out = Vec::new();
    for (params, next) in made {
        let mut told = BTreeSet::new();
        let mut full = params.clone();
        let mut at = vec![0; domains.len()];
        loop {
            for ((name, values), &i) in domains.iter().zip(&at) {
                full.insert(name.as_str(), values[i]);
            }
            if told.insert(after_holds(model, t, &next, &full)) {
                if !budget.hold(full.heap() + next.heap()) || !budget.reserve(&mut out) {
                    return out;
                }
                out.push((full.clone(), next.clone()));
            }
            // The next set of values, the first name turning fastest.
            let Some(turn) = (0..at.len()).find(|&k| at[k] + 1 < domains[k].1.len()) else {
                break;
            };
            at[turn] += 1;
            at[..turn].iter_mut().for_each(|i| *i = 0);
        }
        budget.give(params.heap() + next.heap());
    }
    out
}

/// The parts of what follows a move, one for each trigger guard.
fn parts(after: &Expr) -> &[Expr] {
    match after {
        Expr::And(parts) => parts.as_slice(),
        single => std::slice::from_ref(single),
    }
}

/// The values of `action` a trigger guard reads beside a related record.
/// Such a guard is read in the joint state, where the record decides it, so
/// these values are given to the move as any other is, each kept apart.
pub(crate) fn read_beside_related(automaton: &Automaton, action: &Action) -> BTreeSet<String> {
    let after = after(automaton, action);
    parts(&after)
        .iter()
        .filter(|part| !part.related_types().is_empty() || part.reads_related_field())
        .flat_map(|part| part.params_read())
        .collect()
}

/// Which parts of what follows `t` hold with `params`, on the record `t`
/// left as `next`.
pub(crate) fn after_holds(
    model: &TemperModel,
    t: &ResolvedTransition,
    next: &TemperModelState,
    params: &Fields<ParamValue>,
) -> Vec<u8> {
    let env = MoveEnv {
        local: ModelEnv {
            kinds: &model.var_kinds,
            state: next,
        },
        params,
    };
    parts(&t.after)
        .iter()
        .map(|part| match eval(part, &env) {
            Truth::False => 0,
            Truth::Unknown => 1,
            Truth::True => 2,
        })
        .collect()
}

/// Whether the runtime's copy of `action`'s parameters gives `name` its
/// value: `action` has a parameter of that name, and no state variable of
/// it is one the runtime keeps apart from the fields, or a string the model
/// tracks on its own.
fn copied(automaton: &Automaton, action: &Action, name: &str) -> bool {
    action.params.iter().any(|p| p.name() == name)
        && !matches!(name, "Id" | "id")
        && automaton
            .state
            .iter()
            .filter(|s| s.name == name)
            .all(|s| s.var_type == VarType::Int)
}

fn rewrite(expr: &Expr, copied: &impl Fn(&str) -> bool) -> Expr {
    let operand = |value: &Operand| match value {
        Operand::Var(name) if copied(name) => Operand::Param(name.clone()),
        other => other.clone(),
    };
    match expr {
        Expr::Var(name) if copied(name) => Expr::Compare {
            lhs: Operand::Param(name.clone()),
            op: CmpOp::Eq,
            rhs: Operand::Lit(Literal::Bool(true)),
        },
        Expr::Not(inner) => Expr::Not(Box::new(rewrite(inner, copied))),
        Expr::And(parts) => Expr::And(parts.iter().map(|p| rewrite(p, copied)).collect()),
        Expr::Or(parts) => Expr::Or(parts.iter().map(|p| rewrite(p, copied)).collect()),
        Expr::Implies(a, b) => {
            Expr::Implies(Box::new(rewrite(a, copied)), Box::new(rewrite(b, copied)))
        }
        Expr::Compare { lhs, op, rhs } => Expr::Compare {
            lhs: operand(lhs),
            op: *op,
            rhs: operand(rhs),
        },
        Expr::In {
            value,
            set,
            negated,
        } => Expr::In {
            value: operand(value),
            set: set.clone(),
            negated: *negated,
        },
        other => other.clone(),
    }
}

/// Each bare name in `expr` with the kind its use shows, if any: a
/// condition is a boolean, and a name ordered or compared with a number is
/// a count.
fn uses(expr: &Expr, visit: &mut impl FnMut(&str, Option<ParamKind>)) {
    match expr {
        Expr::Var(name) => visit(name, Some(ParamKind::Bool)),
        Expr::Not(inner) => uses(inner, visit),
        Expr::And(parts) | Expr::Or(parts) => parts.iter().for_each(|p| uses(p, visit)),
        Expr::Implies(a, b) => {
            uses(a, visit);
            uses(b, visit);
        }
        Expr::Compare { lhs, op, rhs } => {
            let kind = |other: &Operand| match other {
                Operand::Lit(Literal::Bool(_)) => Some(ParamKind::Bool),
                Operand::Lit(Literal::Int(_)) => Some(ParamKind::Count),
                _ if !matches!(op, CmpOp::Eq | CmpOp::Ne) => Some(ParamKind::Count),
                _ => None,
            };
            for (value, other) in [(lhs, rhs), (rhs, lhs)] {
                if let Operand::Var(name) = value {
                    visit(name, kind(other));
                }
            }
        }
        Expr::In {
            value: Operand::Var(name),
            ..
        } => visit(name, None),
        _ => {}
    }
}
