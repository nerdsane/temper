//! A reaction's guard that a request's sender decides.
//!
//! A string field that only a trigger's guard reads is not one the model
//! holds, so the model cannot say whether the guard holds and goes both ways.
//! When that field is a parameter of the very request that sets the reaction
//! off, both ways are the sender's to choose: neither is a guess. Anything
//! later that reads the field reads it as unknown, so no sure way leans on a
//! value this choice did not make.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Action, ActionConstraint, Automaton, is_server_derived_field_name};
use temper_spec::predicate::{
    Effect, Env, Expr, Literal, Name, Operand, Set, Truth, Val, VarKind, eval,
};

use super::read::Reaction;

/// The reactions (by place in `reactions`) whose guard the sender of a
/// request that sets them off decides: a request can make it hold and can
/// make it fail.
pub(super) fn chosen_guards(
    by_name: &BTreeMap<&str, &Automaton>,
    reactions: &[Reaction],
) -> BTreeSet<usize> {
    let reads = |reaction: &Reaction| reaction.guard.as_ref().map(fields_read);
    reactions
        .iter()
        .enumerate()
        .filter(|(at, reaction)| {
            let (Some(guard), Some(fields)) = (&reaction.guard, reads(reaction)) else {
                return false;
            };
            let automaton = by_name[reaction.from.as_str()];
            let Some(action) = automaton
                .actions
                .iter()
                .find(|action| action.name == reaction.source_action)
            else {
                return false;
            };
            let Some(fields) = fields.filter(|fields| !fields.is_empty()) else {
                return false;
            };
            // Two guards reading one field are not two free choices.
            let alone = reactions.iter().enumerate().all(|(other, r)| {
                other == *at
                    || r.from != reaction.from
                    || r.source_action != reaction.source_action
                    || !reads(r)
                        .flatten()
                        .is_some_and(|read| read.iter().any(|field| fields.contains(field)))
            });
            alone
                && fields.iter().all(|field| chooses(automaton, action, field))
                && goes_both_ways(guard, &fields, action)
        })
        .map(|(at, _)| at)
        .collect()
}

/// The fields `guard` reads, when it reads only the record's own variables.
fn fields_read(guard: &Expr) -> Option<BTreeSet<String>> {
    let mut fields = BTreeSet::new();
    let mut plain = true;
    guard.for_each_name(&mut |name| match name {
        Name::Var(var) => {
            fields.insert(var.to_string());
        }
        _ => plain = false,
    });
    plain.then_some(fields)
}

/// Whether the sender of `action` sets string `field` to what it likes: it
/// is a parameter the caller supplies, the kernel writes it to the field
/// unchanged, and nothing else in the action reads or writes it.
fn chooses(automaton: &Automaton, action: &Action, field: &str) -> bool {
    let supplied = action
        .params
        .iter()
        .any(|param| param.name() == field && param.source().is_none() && is_string(param));
    let string = automaton
        .state
        .iter()
        .find(|var| var.name == field)
        .is_some_and(|var| var.var_type.kind() == VarKind::Str);
    let only_nonempty = action.constraints.iter().all(|constraint| {
        matches!(constraint, ActionConstraint::ParamNonempty { .. })
            || (constraint.param() != field && constraint.field() != Some(field))
    });
    let mut guard_reads = false;
    action.guard.for_each_name(&mut |name| {
        guard_reads |= matches!(name, Name::Var(var) | Name::Param(var) if var == field);
    });
    let assigned = action
        .effect
        .iter()
        .any(|effect| matches!(effect, Effect::Assign { var, .. } if var == field));
    supplied
        && string
        && only_nonempty
        && !guard_reads
        && !assigned
        && !is_server_derived_field_name(field)
}

fn is_string(param: &temper_spec::automaton::ActionParam) -> bool {
    param.param_type().kind() == VarKind::Str
}

/// Whether some values of `fields` the sender may give make `guard` hold, and
/// some make it fail.
fn goes_both_ways(guard: &Expr, fields: &BTreeSet<String>, action: &Action) -> bool {
    let mut literals = BTreeSet::new();
    literals_of(guard, &mut literals);
    // Longer than any literal, so none of them.
    let other: String = literals.iter().map(String::as_str).chain(["~"]).collect();
    let choices = |field: &str| -> Vec<&str> {
        let nonempty = action.constraints.iter().any(|constraint| {
            matches!(constraint, ActionConstraint::ParamNonempty { param } if param == field)
        });
        literals
            .iter()
            .map(String::as_str)
            .chain([other.as_str(), ""])
            .filter(|value| !nonempty || !value.is_empty())
            .collect()
    };
    let mut picks: Vec<BTreeMap<&str, &str>> = vec![BTreeMap::new()];
    for field in fields {
        picks = picks
            .iter()
            .flat_map(|pick| {
                choices(field).into_iter().map(move |value| {
                    let mut pick = pick.clone();
                    pick.insert(field.as_str(), value);
                    pick
                })
            })
            .collect();
    }
    let truths: Vec<Truth> = picks
        .iter()
        .map(|pick| eval(guard, &Picked(pick)))
        .collect();
    truths.contains(&Truth::True) && truths.contains(&Truth::False)
}

fn literals_of(expr: &Expr, out: &mut BTreeSet<String>) {
    let mut literal = |literal: &Literal| {
        if let Literal::Str(text) = literal {
            out.insert(text.clone());
        }
    };
    match expr {
        Expr::Not(inner) => literals_of(inner, out),
        Expr::And(parts) | Expr::Or(parts) => {
            for part in parts {
                literals_of(part, out);
            }
        }
        Expr::Implies(left, right) => {
            literals_of(left, out);
            literals_of(right, out);
        }
        Expr::Compare { lhs, rhs, .. } => {
            for side in [lhs, rhs] {
                if let Operand::Lit(value) = side {
                    literal(value);
                }
            }
        }
        Expr::In {
            set: Set::List(list),
            ..
        } => list.iter().for_each(literal),
        _ => {}
    }
}

/// The record as one choice of values leaves the fields a guard reads.
struct Picked<'a>(&'a BTreeMap<&'a str, &'a str>);

impl Env for Picked<'_> {
    fn status(&self) -> Val<'_> {
        Val::Unknown
    }

    fn var(&self, name: &str) -> Val<'_> {
        self.0
            .get(name)
            .map_or(Val::Unknown, |value| Val::Str(value))
    }

    fn cross_statuses(&self, _entity_type: &str, _id_field: &str) -> Option<Vec<Val<'_>>> {
        None
    }
}
