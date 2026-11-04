//! The invariant walk: every settled state's invariants, broken only
//! where sure moves alone lead.

use std::collections::BTreeSet;

use super::super::super::{JointOutcome, premise};
use super::super::{JointModel, Walk};
use super::Keep;
use crate::composite::joint::room::Bound;
use temper_spec::predicate::{Expr, Name, Truth, eval};

impl JointModel {
    /// Walk every reachable joint state breadth first, checking `invariants`
    /// of `owner` in each settled one (nothing in flight). An invariant is
    /// broken only where sure moves alone lead: elsewhere the walk may have
    /// gone a way the runtime never goes, so it cannot tell.
    pub(in crate::composite::joint) fn walk(
        &self,
        owner: &str,
        invariants: &[&Expr],
        bound: Bound<'_>,
    ) -> Walk {
        let mut unsupported = BTreeSet::new();
        let modeled = |entity: &str, field: &str| {
            self.models.get(entity).is_some_and(|model| {
                matches!(field, "Id" | "id")
                    || match model.var_kinds.get(field) {
                        Some(temper_spec::predicate::VarKind::Str) => {
                            model.tracked_strings.contains(field)
                        }
                        Some(
                            temper_spec::predicate::VarKind::Counter
                            | temper_spec::predicate::VarKind::Bool
                            | temper_spec::predicate::VarKind::List,
                        ) => true,
                        _ => false,
                    }
            })
        };
        for inv in invariants {
            inv.for_each_name(&mut |name| {
                let mut check = |entity: &str, field: &str| {
                    if !modeled(entity, field) {
                        unsupported.insert(format!("{entity}.{field}"));
                    }
                };
                match name {
                    Name::Var(field) => check(owner, field),
                    Name::CrossStatus { id_field, .. } => check(owner, id_field),
                    Name::CrossField {
                        entity_type,
                        id_field,
                        field,
                    } => {
                        check(owner, id_field);
                        check(entity_type, field);
                    }
                    Name::Status | Name::Param(_) => {}
                }
            });
        }
        if !unsupported.is_empty() {
            let reason = format!(
                "unsupported reads or expansion: {}",
                unsupported.into_iter().collect::<Vec<_>>().join(", ")
            );
            return Walk {
                states: 0,
                held_back: false,
                outside: vec![BTreeSet::new(); invariants.len()],
                outcomes: invariants
                    .iter()
                    .map(|_| JointOutcome::Incomplete {
                        reason: reason.clone(),
                    })
                    .collect(),
            };
        }
        let mut unknown = vec![false; invariants.len()];
        let mut broken: Vec<Option<usize>> = vec![None; invariants.len()];
        // The first state where it is false that sure moves alone may not reach.
        let mut maybe: Vec<Option<usize>> = vec![None; invariants.len()];
        let premises: Vec<Option<&Expr>> = invariants.iter().map(|inv| premise(inv)).collect();
        let mut met: Vec<bool> = premises.iter().map(Option::is_none).collect();
        let explored = self.walk_with(bound, Keep::Sure, |index, state, sure| {
            if state.settled() {
                let env = self.env(owner, state);
                for (i, inv) in invariants.iter().enumerate() {
                    match eval(inv, &env) {
                        Truth::False if sure && broken[i].is_none() => broken[i] = Some(index),
                        Truth::False if maybe[i].is_none() => maybe[i] = Some(index),
                        Truth::Unknown => unknown[i] = true,
                        _ => {}
                    }
                    if let (false, Some(premise)) = (met[i], premises[i]) {
                        met[i] = eval(premise, &env) != Truth::False;
                    }
                }
            }
            broken.iter().all(Option::is_some)
        });
        // The first move each invariant's first maybe-break rests on, and
        // the types outside the scope it comes from when it is a reaction
        // from them.
        let first: Vec<Option<u32>> = maybe
            .iter()
            .map(|found| found.and_then(|index| explored.first_unsure(index)))
            .collect();
        let outside = broken
            .iter()
            .zip(&first)
            .map(|(broken, first)| match (broken, first) {
                (None, Some(label)) => explored
                    .outside_at(*label)
                    .and_then(|(entity, action)| self.outside_from(entity, action))
                    .cloned()
                    .unwrap_or_default(),
                _ => BTreeSet::new(),
            })
            .collect();
        let outcomes = broken
            .iter()
            .zip(&maybe)
            .enumerate()
            .map(|(i, found)| match found {
                (Some(index), _) => JointOutcome::Broken {
                    path: explored.path(*index),
                },
                (None, Some(_)) => JointOutcome::Incomplete {
                    reason: maybe_broken(first[i].map(|label| explored.label(label))),
                },
                (None, None) if unknown[i] => JointOutcome::Incomplete {
                    reason: "an invariant read is unknown".into(),
                },
                (None, None) => match &explored.stopped {
                    Some(reason) => JointOutcome::Incomplete {
                        reason: reason.clone(),
                    },
                    None => match premises[i] {
                        Some(premise) if !met[i] => JointOutcome::Vacuous {
                            premise: premise.to_string(),
                        },
                        _ => JointOutcome::Holds,
                    },
                },
            })
            .collect();
        let (states, held_back) = (explored.len(), explored.held_back);
        // Past where it stopped, phase two (`guided.rs`).
        let outcomes = self.guided_invariants(owner, invariants, outcomes, explored, bound);
        Walk {
            states,
            outcomes,
            held_back,
            outside,
        }
    }
}

/// Why an invariant false only where the walk went by `step`, a move it
/// cannot be sure the runtime makes, is not checked.
fn maybe_broken(step: Option<&str>) -> String {
    format!(
        "it may break, but only on a way through \"{}\", which the runtime may never take: the model cannot decide the Cedar answer, guard or reference behind it",
        step.unwrap_or("a move the walk cannot be sure of")
    )
}
