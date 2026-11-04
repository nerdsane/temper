//! What a walk holds, counted against its room: its tables, and what each
//! expansion builds before the walk keeps it.

use std::collections::{HashMap, VecDeque};

use super::super::JointState;
use super::super::pack::Packed;
use super::super::room::{Heap, Held, bytes, grow, grow_map, map_bytes};
use super::JointModel;
use super::step::{Certainty, Move, Next};
use super::walk::Explored;

impl Heap for Certainty {
    fn heap(&self) -> usize {
        0
    }
}

/// The walk's tables as the memory it holds counts them: each at its
/// capacity, and each packed state as its own heap block. Before each state
/// is walked, every table is grown to hold what walking one state has added
/// at most so far, with the memory of its new copy taken first, so a table
/// seldom grows on its own while both copies are held.
#[derive(Default)]
pub(super) struct Tables {
    /// Bytes the packed states take.
    pub(super) states: usize,
    /// The most states, moves and edges walking one state has added.
    pub(super) most: [usize; 3],
    /// Bytes given to the expansions under way, for what they build.
    building: usize,
    /// Bytes the senders' answers decided again hold.
    pub(super) decided: usize,
}

/// The room an expansion is first given; it doubles until what the
/// expansion builds fits.
const BUILD_ROOM: usize = 64 << 10;

impl JointModel {
    /// Run `build` with room for what it builds taken from the walk's,
    /// doubling the room and building again until it fits. The room stays
    /// taken, as the second value, until [`Tables::give`] gives it back.
    pub(super) fn within<T>(
        &self,
        held: &mut Held<'_>,
        tables: &mut Tables,
        build: impl Fn() -> T,
    ) -> Result<(T, usize), String> {
        let mut room = BUILD_ROOM;
        loop {
            held.add(room)
                .map_err(|reason| format!("{reason} for what one move builds"))?;
            self.expansion.allow(room);
            let built = build();
            let short = self.expansion.short();
            self.expansion.allow(usize::MAX);
            if !short {
                tables.building += room;
                return Ok((built, room));
            }
            drop(built);
            held.sub(room);
            room = room.saturating_mul(2);
        }
    }
}

impl Tables {
    /// Give back `room` an expansion was given.
    pub(super) fn give(&mut self, held: &mut Held<'_>, room: usize) {
        self.building -= room;
        held.sub(room);
    }

    /// Make the tables hold `states` new states and, with `edges`, one
    /// more move and an edge for each outcome.
    pub(super) fn reserve(
        &mut self,
        explored: &mut Explored<'_>,
        seen: &mut HashMap<Packed, u32>,
        queue: &mut VecDeque<usize>,
        edges: bool,
        states: usize,
        outcomes: usize,
    ) -> Result<(), String> {
        let held = &mut explored.held;
        grow(&mut explored.nodes, states, held)?;
        grow(queue, states, held)?;
        grow_map(seen, states, held)?;
        if edges {
            grow(&mut explored.made, 1, held)?;
            grow(&mut explored.edges, outcomes, held)?;
        }
        Ok(())
    }

    /// Count afresh what the walk holds: its tables, and the room given to
    /// the expansions under way.
    pub(super) fn count(
        &self,
        explored: &mut Explored<'_>,
        seen: &HashMap<Packed, u32>,
        queue: &VecDeque<usize>,
    ) -> Result<(), String> {
        let bytes = self.states
            + explored.packer.bytes()
            + bytes(&explored.nodes)
            + bytes(queue)
            + map_bytes::<Packed, u32>(seen.capacity())
            + bytes(&explored.made)
            + bytes(&explored.edges)
            + self.building
            + self.decided;
        explored.held.set(bytes)
    }

    pub(super) fn saw(&mut self, added: [usize; 3]) {
        for (most, added) in self.most.iter_mut().zip(added) {
            *most = (*most).max(added);
        }
    }

    pub(super) fn make_room(
        &mut self,
        explored: &mut Explored<'_>,
        seen: &mut HashMap<Packed, u32>,
        queue: &mut VecDeque<usize>,
        edges: bool,
    ) -> Result<(), String> {
        let [states, made, ends] = self.most.map(|most| most.max(1));
        let held = &mut explored.held;
        grow(&mut explored.nodes, states, held)?;
        grow(queue, states, held)?;
        grow_map(seen, states, held)?;
        if edges {
            grow(&mut explored.made, made, held)?;
            grow(&mut explored.edges, ends, held)?;
        }
        self.count(explored, seen, queue)
    }
}

impl Heap for Next {
    fn heap(&self) -> usize {
        self.state.heap() + self.renamed.heap() + self.label.heap()
    }
}

impl Heap for Move {
    fn heap(&self) -> usize {
        match self {
            Move::Take(entity, action, _)
            | Move::Send(entity, action, _)
            | Move::Reaction(entity, action, _) => entity.heap() + action.heap(),
            Move::Timer(entity, name) => entity.heap() + name.heap(),
            Move::Answer(call, action) => call.heap() + action.heap(),
            Move::Fail(call) | Move::Lost(call) | Move::Silent(call) => call.heap(),
            Move::Read(dispatch) | Move::Arrive(dispatch) => dispatch.heap(),
        }
    }
}

impl JointModel {
    /// Keep `state` among the ways a commit's reactions go, counting it
    /// against the expansion's room: each reaction whose guard the model
    /// cannot decide doubles them. False when the room is used up.
    pub(super) fn keep(
        &self,
        after: &mut Vec<(JointState, Certainty)>,
        state: JointState,
        known: Certainty,
    ) -> bool {
        self.expansion.push(after, (state, known))
    }

    /// Keep `mv` among a state's moves, counting it against the expansion's
    /// room. False when the room is used up.
    pub(super) fn keep_move(&self, out: &mut Vec<Move>, mv: Move) -> bool {
        self.expansion.push(out, mv)
    }
}

#[cfg(test)]
#[path = "held_test.rs"]
mod tests;
