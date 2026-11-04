//! Turning a fair component into a behaviour someone can read.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::super::JointStep;
use super::super::liveness::sure_path;
use super::super::model::{Explored, JointModel};
use super::super::state::render;
use super::Lasso;
use super::graph::Graph;
use super::search::View;

/// Something a fair cycle has to pass through.
#[derive(Clone, Copy)]
enum Witness {
    /// A move a constraint is satisfied by taking.
    Way(u32),
    /// A state where a weakly fair move is not possible.
    State(u32),
}

/// The behaviour a fair component stands for: the sure path to its first state,
/// then a closed walk through it that satisfies each constraint.
pub(super) fn lasso(
    model: &JointModel,
    explored: &Explored,
    view: &View,
    parents: &[Option<u32>],
    component: &[u32],
) -> Lasso {
    let root = component[0];
    Lasso {
        prefix: sure_path(model, explored, parents, root as usize),
        cycle: closed_walk(view, component, root)
            .into_iter()
            .map(|edge| step(model, view.graph, edge))
            .collect(),
    }
}

/// Name one way of the walk by its move, by expanding the state it leaves.
fn step(model: &JointModel, graph: &Graph, edge: u32) -> JointStep {
    let explored = graph.explored;
    let edge = &explored.edges[edge as usize];
    let made = &explored.made[edge.made as usize];
    let before = explored.state(made.from as usize);
    let after = explored.state(edge.to as usize);
    let key = &graph.keys[graph.key_of(edge.made) as usize];
    let action = model
        .moves(&before)
        .iter()
        .filter(|mv| &model.fair_key(&before, mv) == key)
        .flat_map(|mv| model.next(&before, mv).0)
        .find(|next| next.state == after)
        .map(|next| next.label);
    JointStep {
        action,
        state: render(&after),
        unsure: false,
    }
}

/// A walk from `root` back to it inside `component`, by edge, that passes each
/// witness. Empty when staying at `root` is already fair.
fn closed_walk(view: &View, component: &[u32], root: u32) -> Vec<u32> {
    let members: BTreeSet<u32> = component.iter().copied().collect();
    let explored = view.graph.explored;
    let mut walk: Vec<u32> = Vec::new();
    let mut visited = BTreeSet::from([root]);
    let mut at = root;
    for witness in witnesses(view, component, &members) {
        match witness {
            Witness::Way(edge) if walk.contains(&edge) => {}
            Witness::Way(edge) => {
                let made = &explored.made[explored.edges[edge as usize].made as usize];
                let way = path(view, &members, at, made.from);
                note(explored, &way, &mut visited);
                walk.extend(way);
                walk.push(edge);
                at = explored.edges[edge as usize].to;
                visited.insert(at);
            }
            Witness::State(state) if visited.contains(&state) => {}
            Witness::State(state) => {
                let way = path(view, &members, at, state);
                note(explored, &way, &mut visited);
                walk.extend(way);
                at = state;
            }
        }
    }
    walk.extend(path(view, &members, at, root));
    walk
}

fn note(explored: &Explored, way: &[u32], visited: &mut BTreeSet<u32>) {
    visited.extend(way.iter().map(|&edge| explored.edges[edge as usize].to));
}

/// For each constraint with a possible move in the component, what satisfies it.
fn witnesses(view: &View, component: &[u32], members: &BTreeSet<u32>) -> Vec<Witness> {
    #[derive(Default)]
    struct Seen {
        way: Option<u32>,
        possible_at: BTreeSet<u32>,
    }
    let mut seen: BTreeMap<u32, Seen> = BTreeMap::new();
    for &state in component {
        for &row in view.graph.rows_of(state) {
            let possible = view.possible(row);
            let way = view
                .graph
                .edges_of(row)
                .find(|(_, edge)| view.taken(row, edge) && members.contains(&edge.to))
                .map(|(id, _)| id);
            for constraint in view.constraints_of(row) {
                let entry = seen.entry(constraint).or_default();
                entry.way = entry.way.or(way);
                if possible {
                    entry.possible_at.insert(state);
                }
            }
            if let Some(timer) = view.timer_reset_by(row) {
                let entry = seen.entry(timer).or_default();
                entry.way = entry.way.or(way);
            }
        }
    }
    seen.values()
        .filter(|entry| !entry.possible_at.is_empty())
        .filter_map(|entry| match entry.way {
            Some(way) => Some(Witness::Way(way)),
            // A component judged fair has such a state for a weak constraint,
            // and a strong one with no way taken is possible nowhere in it.
            None => component
                .iter()
                .find(|state| !entry.possible_at.contains(state))
                .map(|&state| Witness::State(state)),
        })
        .collect()
}

/// A shortest path of edges from `from` to `to`, staying among `members`.
fn path(view: &View, members: &BTreeSet<u32>, from: u32, to: u32) -> Vec<u32> {
    if from == to {
        return Vec::new();
    }
    let mut came: BTreeMap<u32, (u32, u32)> = BTreeMap::new();
    let mut queue = VecDeque::from([from]);
    while let Some(state) = queue.pop_front() {
        for &row in view.graph.rows_of(state) {
            for (id, edge) in view.graph.edges_of(row) {
                let next = edge.to;
                if !view.usable_edge(row, edge)
                    || !members.contains(&next)
                    || next == from
                    || came.contains_key(&next)
                {
                    continue;
                }
                came.insert(next, (state, id));
                if next == to {
                    let mut walk = Vec::new();
                    let mut at = to;
                    while at != from {
                        let (before, edge) = came[&at];
                        walk.push(edge);
                        at = before;
                    }
                    walk.reverse();
                    return walk;
                }
                queue.push_back(next);
            }
        }
    }
    Vec::new()
}

/// Why a fair cycle that may exist is not known to: the moves it rests on that
/// the model cannot decide, or the first such move on the way to it.
pub(super) fn unsure_reason(explored: &Explored, view: &View, component: &[u32]) -> String {
    let mut unsure: BTreeSet<String> = BTreeSet::new();
    for &state in component {
        for &row in view.graph.rows_of(state) {
            if !view.usable(row) {
                continue;
            }
            let made = &explored.made[row as usize];
            if made.outside || view.graph.edges_of(row).any(|(_, edge)| !edge.sure) {
                let key = view.graph.key_of(row) as usize;
                unsure.insert(view.graph.keys[key].to_string());
            }
        }
    }
    let root = component[0] as usize;
    let on_the_way = explored
        .first_unsure(root)
        .map(|label| format!(" The way there goes through \"{}\".", explored.label(label)))
        .unwrap_or_default();
    let listed: Vec<String> = unsure.iter().take(6).cloned().collect();
    let more = unsure.len() - listed.len();
    let rests = if listed.is_empty() {
        "the way to it or the moves that may be possible on it".to_string()
    } else if more > 0 {
        format!("{} and {more} more", listed.join(", "))
    } else {
        listed.join(", ")
    };
    format!(
        "a fair cycle of up to {} joint states may keep it from the condition, starting at {}, \
         but that rests on what the model cannot decide: {rests}.{on_the_way}",
        component.len(),
        render(&explored.state(root))
    )
}
