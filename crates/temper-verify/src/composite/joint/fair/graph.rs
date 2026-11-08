//! The found graph as the fairness check reads it.

use std::collections::BTreeMap;

use super::super::model::{Edge, Explored, FairKey, JointModel};

/// The found graph, indexed by state and by move, and which move each one is.
/// A walk on many threads leaves [`Explored`]'s lists in no order but this: a
/// state's moves stay in the order [`JointModel::moves`] gives them.
pub(super) struct Graph<'a> {
    pub(super) explored: &'a Explored<'a>,
    rows_at: Vec<u32>,
    rows: Vec<u32>,
    edges_at: Vec<u32>,
    out: Vec<u32>,
    /// Each distinct move.
    pub(super) keys: Vec<FairKey>,
    /// By move made: its index in `keys`.
    key_of: Vec<u32>,
}

impl<'a> Graph<'a> {
    /// The graph of a finished walk. The keys are worked out here, after the
    /// walk and on one thread, so a walk costs the same whether or not a rule
    /// asks for fairness. An error says a state no longer has the moves the
    /// walk made from it.
    pub(super) fn new(model: &JointModel, explored: &'a Explored<'a>) -> Result<Self, String> {
        let group = |keys: Vec<usize>, groups: usize| {
            let mut at = vec![0u32; groups + 1];
            for &key in &keys {
                at[key + 1] += 1;
            }
            for index in 0..groups {
                at[index + 1] += at[index];
            }
            let mut next = at.clone();
            let mut items = vec![0u32; keys.len()];
            for (item, &key) in keys.iter().enumerate() {
                items[next[key] as usize] = item as u32;
                next[key] += 1;
            }
            (at, items)
        };
        let (rows_at, rows) = group(
            explored.made.iter().map(|m| m.from as usize).collect(),
            explored.len(),
        );
        let (edges_at, out) = group(
            explored.edges.iter().map(|e| e.made as usize).collect(),
            explored.made.len(),
        );
        let mut graph = Self {
            explored,
            rows_at,
            rows,
            edges_at,
            out,
            keys: Vec::new(),
            key_of: vec![0; explored.made.len()],
        };
        let mut known: BTreeMap<FairKey, u32> = BTreeMap::new();
        for state in 0..explored.len() {
            let rows = graph.rows_at[state] as usize..graph.rows_at[state + 1] as usize;
            if rows.is_empty() {
                continue;
            }
            let state_now = explored.state(state);
            let moves = model.moves(&state_now);
            if moves.len() != rows.len() {
                return Err(format!(
                    "state {state} has {} moves on a second look, and the walk made {}",
                    moves.len(),
                    rows.len()
                ));
            }
            for (row, mv) in rows.zip(&moves) {
                let key = model.fair_key(&state_now, mv);
                let id = *known.entry(key).or_insert_with_key(|key| {
                    graph.keys.push(key.clone());
                    graph.keys.len() as u32 - 1
                });
                graph.key_of[graph.rows[row] as usize] = id;
            }
        }
        Ok(graph)
    }

    /// Which move the move made `row` is, in `keys`.
    pub(super) fn key_of(&self, row: u32) -> u32 {
        self.key_of[row as usize]
    }

    /// The moves made from `state`.
    pub(super) fn rows_of(&self, state: u32) -> &[u32] {
        let state = state as usize;
        &self.rows[self.rows_at[state] as usize..self.rows_at[state + 1] as usize]
    }

    /// The ways the move `row` leads.
    pub(super) fn edges_of(&self, row: u32) -> impl Iterator<Item = (u32, &Edge)> {
        let row = row as usize;
        self.out[self.edges_at[row] as usize..self.edges_at[row + 1] as usize]
            .iter()
            .map(|&edge| (edge, &self.explored.edges[edge as usize]))
    }
}
