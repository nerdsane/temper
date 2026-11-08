//! Asking whether what happened in production is a path of the spec.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Expr, Truth, eval};

use super::JointStep;
use super::model::{JointModel, Move, Sending};
use super::state::{Dispatch, render};
use crate::model::{Fields, ModelString, ParamValue};

/// One move of a trace: an entity's action with the string parameters it
/// was sent. Values are names for this trace only: equal names are equal
/// values, `''` is the empty string, and a name not seen before is a value
/// nothing holds yet. A parameter the model does not track is not checked.
#[derive(Debug, Clone)]
pub struct TraceMove {
    /// The entity type.
    pub entity: String,
    /// The action.
    pub action: String,
    /// String parameters by name; parameters left out may take any value.
    pub params: BTreeMap<String, String>,
}

/// A trace walked through the joint model of `entity`'s cross-entity
/// invariants.
#[derive(Debug, Clone)]
pub struct JointReplay {
    /// The start and the state after each step.
    pub steps: Vec<JointStep>,
    /// The invariants false in the last state.
    pub broken: Vec<String>,
}

/// Walk `trace` through the joint model of `entity`'s invariants that read
/// related entities, as the check does: whether the spec allows each move,
/// and which invariants the last state breaks. `ids` names each record's id
/// in the trace's names (equal names, equal ids); a type left out gets an
/// id of its own. A move the trace names takes a dispatch in flight to that
/// action when there is one, and is otherwise sent from outside; when it is
/// not allowed yet, dispatches in flight land first, oldest first. A
/// reaction whose guard the model cannot decide is taken as not firing. Use
/// it to ask whether what happened in production is a path of the spec.
pub fn replay_joint_invariants(
    automatons: &[&Automaton],
    entity: &str,
    ids: &BTreeMap<String, String>,
    trace: &[TraceMove],
) -> Result<JointReplay, String> {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    let automaton = by_name
        .get(entity)
        .ok_or_else(|| format!("no entity type {entity}"))?;
    let joint: Vec<&temper_spec::automaton::Invariant> = automaton
        .invariants
        .iter()
        .filter(|inv| !inv.assert.related_types().is_empty())
        .collect();
    let mut scope = BTreeSet::from([entity.to_string()]);
    for inv in &joint {
        for (entity_type, _) in inv.assert.related_types() {
            if !by_name.contains_key(entity_type.as_str()) {
                return Err(format!(
                    "{entity} reads {entity_type}, which is not declared"
                ));
            }
            scope.insert(entity_type);
        }
    }
    let exprs: Vec<&Expr> = joint.iter().map(|inv| &inv.assert).collect();
    let model = JointModel::new(&by_name, &scope, entity, &exprs, Sending::anyone(), true);
    let (mut state, mut names) = model.init_with_ids(ids);
    let mut steps = vec![JointStep {
        action: None,
        state: render(&state),
        unsure: false,
    }];
    for (i, wanted) in trace.iter().enumerate() {
        loop {
            let fits = |params: &Fields<ParamValue>| {
                let mut bindings = names.clone();
                let mut used = model.values(&state);
                wanted.params.iter().all(|(param, name)| {
                    // A parameter the model does not track takes any value.
                    let Some(ParamValue::Str(value)) = params.get(param) else {
                        return true;
                    };
                    let name = (model.class_of(&wanted.entity, param), name.clone());
                    match bindings.get(&name) {
                        Some(bound) => value == bound,
                        None if name.1.is_empty() => value.is_empty(),
                        // A value naming no record may be the value others are.
                        None if value.symbol_id().is_some()
                            && (value.number() == Some(0) || !used.contains(value)) =>
                        {
                            bindings.insert(name, *value);
                            used.insert(*value);
                            true
                        }
                        None => false,
                    }
                })
            };
            let wanted_dispatch = |d: &Dispatch| {
                d.to == wanted.entity && d.action == wanted.action && fits(&d.params)
            };
            let moves = model.moves(&state);
            let named = moves
                .iter()
                .find(|mv| match mv {
                    Move::Read(d) | Move::Arrive(d) => wanted_dispatch(d),
                    _ => false,
                })
                .or_else(|| {
                    moves.iter().find(|mv| match mv {
                        Move::Take(e, action, _) | Move::Send(e, action, _) => {
                            e == &wanted.entity
                                && action.name == wanted.action
                                && fits(&action.params)
                        }
                        _ => false,
                    })
                });
            let (mv, done) = match named {
                Some(mv @ (Move::Take(..) | Move::Arrive(_))) => (mv.clone(), true),
                Some(mv) => (mv.clone(), false),
                None => match moves
                    .iter()
                    .find(|mv| matches!(mv, Move::Read(_) | Move::Arrive(_)))
                {
                    Some(mv) => (mv.clone(), false),
                    None => {
                        return Err(format!(
                            "move {} ({}.{}) is not allowed in {}",
                            i + 1,
                            wanted.entity,
                            wanted.action,
                            render(&state)
                        ));
                    }
                },
            };
            if let Move::Take(_, action, _) | Move::Send(_, action, _) = &mv {
                for (param, name) in &wanted.params {
                    if let Some(ParamValue::Str(value)) = action.params.get(param)
                        && !name.is_empty()
                    {
                        let class = model.class_of(&wanted.entity, param);
                        names.entry((class, name.clone())).or_insert_with(|| *value);
                    }
                }
            }
            let next = model
                .next(&state, &mv)
                .0
                .into_iter()
                .next()
                .ok_or_else(|| {
                    format!(
                        "move {} ({}.{}) cannot be taken",
                        i + 1,
                        wanted.entity,
                        wanted.action
                    )
                })?;
            names = names
                .into_iter()
                .filter_map(|(name, value)| match next.renamed.get(&value) {
                    Some(new) => Some((name, *new)),
                    None if matches!(value, ModelString::Literal(_)) => Some((name, value)),
                    None => None,
                })
                .collect();
            steps.push(JointStep {
                action: Some(next.label),
                state: render(&next.state),
                unsure: false,
            });
            state = next.state;
            if done {
                break;
            }
        }
    }
    let env = model.env(entity, &state);
    let broken = joint
        .iter()
        .filter(|inv| eval(&inv.assert, &env) == Truth::False)
        .map(|inv| inv.name.clone())
        .collect();
    Ok(JointReplay { steps, broken })
}
