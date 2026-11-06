//! What a stuck verdict may say: it refuses the install, so it rests on what
//! the runtime is known to do, and what is less sure is not checked.

use std::collections::{BTreeSet, VecDeque};

use super::super::model::{self, Certainty, Explored, JointModel, Made};
use super::super::state::render;
use super::super::{JointState, JointStep};

/// Which walked states any move the system makes may lead to the goal from:
/// every way it may go counts, sure or not, and so does a move the walk held
/// a way of back.
pub(super) fn may_reach(explored: &Explored, done: &[bool], failing: &[bool]) -> Vec<bool> {
    let usable = |made: &Made| {
        made.by_system || made.outside || (made.relied && failing[made.from as usize])
    };
    let mut from: Vec<Vec<u32>> = vec![Vec::new(); explored.len()];
    for edge in &explored.edges {
        let made = &explored.made[edge.made as usize];
        if usable(made) {
            from[edge.to as usize].push(made.from);
        }
    }
    let mut good = done.to_vec();
    for made in explored
        .made
        .iter()
        .filter(|made| made.held && usable(made))
    {
        good[made.from as usize] = true;
    }
    let mut queue: VecDeque<usize> = (0..good.len()).filter(|&index| good[index]).collect();
    while let Some(index) = queue.pop_front() {
        for &before in &from[index] {
            if !good[before as usize] {
                good[before as usize] = true;
                queue.push_back(before as usize);
            }
        }
    }
    good
}

/// Why no state the goal is not sure from is known to be stuck: the first
/// that no move may lead on from rests on a move the runtime may never make,
/// or else every one of them has a way on the model cannot decide.
pub(super) fn unsure_reason(
    model: &JointModel,
    explored: &Explored,
    good: &[bool],
    out: &[bool],
) -> String {
    let count = explored.len();
    let unsure = good.iter().filter(|&&g| !g).count();
    let first = (0..count).find(|&index| !out[index]);
    match first
        .and_then(|index| explored.first_unsure(index))
        .and_then(|index| explored.label(model, index))
    {
        Some(label) => format!(
            "it may never settle, but only on a way through \"{}\", which the runtime may never \
             take: the model cannot decide the Cedar answer, guard or reference behind it",
            label
        ),
        None => format!(
            "from {unsure} of {count} joint states the system's moves are not sure to reach the \
             condition, and each may still get there by a move whose guard, Cedar answer or \
             parameters the model cannot decide"
        ),
    }
}

/// The moves sure moves alone reach the state walked `index`th by.
pub(in crate::composite::joint) fn sure_path(
    model: &JointModel,
    explored: &Explored,
    parents: &[Option<u32>],
    mut index: usize,
) -> Vec<JointStep> {
    let mut states = vec![explored.state(index)];
    while let Some(from) = parents[index].filter(|&from| from as usize != index) {
        index = from as usize;
        states.push(explored.state(index));
    }
    states.reverse();
    sure_steps(model, &states)
}

/// `states`, from a start, each reached from the one before by a sure move:
/// the moves, in words.
pub(in crate::composite::joint) fn sure_steps(
    model: &JointModel,
    states: &[JointState],
) -> Vec<JointStep> {
    let mut steps: Vec<JointStep> = Vec::with_capacity(states.len());
    for (at, state) in states.iter().enumerate() {
        let action = at.checked_sub(1).map(|before| {
            let before = &states[before];
            model
                .moves(before)
                .iter()
                .flat_map(|mv| model.next(before, mv).0)
                .find(|next| next.certainty == Certainty::Sure && next.state == *state)
                .map(|next| next.label)
                .unwrap_or_default()
        });
        steps.push(JointStep {
            action,
            state: render(state),
            unsure: false,
        });
    }
    steps
}

/// The moves the system makes from a stuck `state`, in words; with `failing`,
/// those outside code is relied on to send too.
pub(super) fn system_moves(model: &JointModel, state: &JointState, failing: bool) -> Vec<String> {
    let mut moves = BTreeSet::new();
    for mv in model.moves(state) {
        if !(mv.by_system() || failing && model.relies(&mv)) {
            continue;
        }
        for next in model.next(state, &mv).0 {
            moves.insert(next.label);
        }
    }
    moves.into_iter().collect()
}

/// The reactions from types outside the scope that may be taken in `state`,
/// as `Entity.Action (from Status) by a reaction from Type`.
pub(super) fn outside_moves(model: &JointModel, state: &JointState) -> Vec<String> {
    model
        .moves(state)
        .into_iter()
        .filter_map(|mv| match mv {
            model::Move::Reaction(entity, action, _) => Some(format!(
                "{entity}.{} (from {}) by a reaction from {}",
                action.name,
                state.records[entity.as_str()].status,
                model
                    .outside_from(&entity, &action.name)
                    .map(|from| from
                        .iter()
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                        .join(" or "))
                    .unwrap_or_default()
            )),
            _ => None,
        })
        .collect()
}
