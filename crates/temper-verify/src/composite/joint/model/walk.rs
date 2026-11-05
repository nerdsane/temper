//! What the walk over joint states finds (`walk/run.rs` walks them).

use std::collections::{BTreeSet, VecDeque};
use std::sync::atomic::{AtomicU8, Ordering};

use super::super::pack::{Packed, Packer};
use super::super::room::{Held, MB};
use super::super::state::render;
use super::super::{JointOutcome, JointState, JointStep};
use super::JointModel;
use super::step::Next;

mod checked;
mod conditions;
mod invariants;
mod levels;
mod making;
mod memo;
mod mirror;
mod nodes;
mod run;
mod shared;

/// What a walk found.
pub(in crate::composite::joint) struct Walk {
    /// Distinct joint states walked.
    pub(in crate::composite::joint) states: usize,
    /// One per invariant.
    pub(in crate::composite::joint) outcomes: Vec<JointOutcome>,
    /// Whether a move was held back for [`super::MAX_IN_FLIGHT`].
    pub(in crate::composite::joint) held_back: bool,
    /// One per invariant: when it is false only where the walk went by a
    /// reaction from types outside the scope, those types, else none.
    pub(in crate::composite::joint) outside: Vec<BTreeSet<String>>,
}

/// One move made from a walked state.
#[derive(Debug, Clone, Copy)]
pub(in crate::composite::joint) struct Made {
    pub(in crate::composite::joint) from: u32,
    /// The system made it: a timer, a module, or the kernel's dispatches.
    pub(in crate::composite::joint) by_system: bool,
    /// It sends an action outside code is relied on to send.
    pub(in crate::composite::joint) relied: bool,
    /// It is a reaction from a type outside the scope.
    pub(in crate::composite::joint) outside: bool,
    /// It always ends in one of the states its edges reach: it leads
    /// somewhere, none of its states is unsure, and none was held back.
    pub(in crate::composite::joint) covered: bool,
    /// The walk held back a way it may go, over the bound on what is in flight.
    pub(in crate::composite::joint) held: bool,
}

/// A move's way from one walked state to another.
#[derive(Debug, Clone, Copy)]
pub(in crate::composite::joint) struct Edge {
    /// The move, an index into [`Explored::made`].
    pub(in crate::composite::joint) made: u32,
    pub(in crate::composite::joint) to: u32,
    /// The move can lead here.
    pub(in crate::composite::joint) sure: bool,
}

/// What a walk keeps beyond each state and the way it first reached it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::composite::joint) enum Keep {
    /// Nothing more.
    States,
    /// Every move made from a walked state, and every way it leads.
    Edges,
    /// Which states sure moves alone reach from a start. A state first
    /// reached otherwise, and later by a sure move from such a state, is
    /// walked again, so the states its sure moves lead to learn it too.
    Sure,
}

/// How a state was reached from the state before it. Words for the move are
/// made from this when a trace prints it (a move and its outcomes are the same
/// whenever they are worked out from the same state).
#[derive(Debug, Clone, Copy)]
pub(super) struct Via {
    /// The move, an index into the moves of the state before.
    pub(super) by: u32,
    /// The state, an index into what the move leads to.
    pub(super) outcome: u32,
}

/// A state walked.
pub(super) struct Node {
    pub(super) packed: Packed,
    /// The state it was first reached from and the way, or once sure moves
    /// alone reach it, the first sure one.
    pub(super) parent: Option<(u32, Via)>,
    /// Whether sure moves alone reach it, and whether it waits to be walked.
    /// One word, so a thread starting to walk it and one finding a sure way
    /// to it cannot both miss the other: either the walk starts sure, or the
    /// state is walked again.
    flags: AtomicU8,
}

const SURE: u8 = 1;
const QUEUED: u8 = 2;

impl Node {
    pub(super) fn new(
        packed: Packed,
        parent: Option<(u32, Via)>,
        sure: bool,
        queued: bool,
    ) -> Self {
        let flags = if sure { SURE } else { 0 } | if queued { QUEUED } else { 0 };
        Self {
            packed,
            parent,
            flags: AtomicU8::new(flags),
        }
    }

    /// Sure moves alone reach it from a start.
    pub(super) fn sure(&self) -> bool {
        self.flags.load(Ordering::Acquire) & SURE != 0
    }

    /// It waits in the walk's queue.
    fn queued(&self) -> bool {
        self.flags.load(Ordering::Acquire) & QUEUED != 0
    }

    /// Start walking it: it no longer waits. Whether sure moves alone reach it.
    fn start(&self) -> bool {
        self.flags.fetch_and(!QUEUED, Ordering::AcqRel) & SURE != 0
    }

    /// Sure moves alone reach it now. Whether it must be walked again: it
    /// does not wait to be walked already.
    fn found_sure(&self) -> bool {
        self.flags.fetch_or(SURE | QUEUED, Ordering::AcqRel) & QUEUED == 0
    }
}

/// What a walk saw, and the memory it holds for it.
pub(in crate::composite::joint) struct Explored<'a> {
    /// Each state walked.
    pub(super) nodes: Vec<Node>,
    pub(super) packer: Packer,
    /// Every move made from a walked state, when asked for.
    pub(in crate::composite::joint) made: Vec<Made>,
    /// Every way those moves lead between walked states.
    pub(in crate::composite::joint) edges: Vec<Edge>,
    /// Why the walk stopped before seeing every state.
    pub(in crate::composite::joint) stopped: Option<String>,
    /// Whether a move was held back for [`super::MAX_IN_FLIGHT`].
    pub(in crate::composite::joint) held_back: bool,
    /// How many moves from a start it covered: it walked every state that near.
    pub(in crate::composite::joint) covered: usize,
    /// How many moves it was working at when it ended.
    pub(in crate::composite::joint) reached: usize,
    /// The memory its tables hold, given back when it is dropped.
    pub(in crate::composite::joint) held: Held<'a>,
}

impl Explored<'_> {
    /// Stop the walk: its room said no.
    fn stop_for_room(&mut self, reason: String) {
        self.stopped = Some(format!(
            "the walk does not fit in the memory it may hold: {reason} \
             (it held {} MB at {} states, having walked every state within {} moves of a start \
             and working at about {} moves)",
            self.held.bytes / MB,
            self.len(),
            self.covered,
            self.reached
        ));
    }

    /// How many states the walk saw.
    pub(in crate::composite::joint) fn len(&self) -> usize {
        self.nodes.len()
    }

    /// The state walked `index`th.
    pub(in crate::composite::joint) fn state(&self, index: usize) -> JointState {
        self.packer.unpack(&self.nodes[index].packed)
    }

    /// The moves from the start to the state walked `index`th.
    pub(in crate::composite::joint) fn path(
        &self,
        model: &JointModel,
        mut index: usize,
    ) -> Vec<JointStep> {
        let mut steps = Vec::new();
        loop {
            let parent = self.nodes[index].parent;
            steps.push(JointStep {
                action: self.label(model, index),
                state: render(&self.state(index)),
                unsure: false,
            });
            match parent {
                Some((from, _)) => index = from as usize,
                None => break,
            }
        }
        steps.reverse();
        steps
    }

    /// For each state sure moves alone reach from a start, the state they
    /// come to it from, a start naming itself. Only a walk that keeps its
    /// edges knows this.
    pub(in crate::composite::joint) fn sure_parents(&self) -> Vec<Option<u32>> {
        let mut parents: Vec<Option<u32>> = vec![None; self.nodes.len()];
        let mut out: Vec<Vec<u32>> = vec![Vec::new(); self.nodes.len()];
        for edge in self.edges.iter().filter(|edge| edge.sure) {
            out[self.made[edge.made as usize].from as usize].push(edge.to);
        }
        let mut queue = VecDeque::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if node.parent.is_none() {
                parents[index] = Some(index as u32);
                queue.push_back(index);
            }
        }
        while let Some(index) = queue.pop_front() {
            for &to in &out[index] {
                if parents[to as usize].is_none() {
                    parents[to as usize] = Some(index as u32);
                    queue.push_back(to as usize);
                }
            }
        }
        parents
    }

    /// The first move on the way to the state walked `index`th that the
    /// walk cannot be sure the runtime makes, when there is one: the move
    /// into the first state on it that sure moves alone do not reach. It is
    /// named by the state it leads to ([`Self::label`]).
    pub(in crate::composite::joint) fn first_unsure(&self, mut index: usize) -> Option<usize> {
        let mut first = None;
        while let Some((from, _)) = self.nodes[index].parent {
            if !self.nodes[index].sure() && self.nodes[from as usize].sure() {
                first = Some(index);
            }
            index = from as usize;
        }
        first
    }

    /// The move the walk reached the state walked `index`th by, and what it
    /// made of it, worked out again. None for a starting state.
    fn way_into(&self, model: &JointModel, index: usize) -> Option<(super::Move, Next)> {
        let (from, via) = self.nodes[index].parent?;
        let before = self.state(from as usize);
        let (mv, next) = model.expansion.outside_the_count(|| {
            let mut moves = model.moves(&before);
            let mv = moves.swap_remove(via.by as usize);
            let next = model.next(&before, &mv).0.swap_remove(via.outcome as usize);
            (mv, next)
        });
        assert!(
            next.state == self.state(index),
            "a move leads where it led when the walk made it"
        );
        Some((mv, next))
    }

    /// The move the walk reached the state walked `index`th by, in words.
    /// None for a starting state.
    pub(in crate::composite::joint) fn label(
        &self,
        model: &JointModel,
        index: usize,
    ) -> Option<String> {
        self.way_into(model, index).map(|(_, next)| next.label)
    }

    /// The type and action the move into the state walked `index`th sends,
    /// when it is a reaction from a type outside the scope.
    pub(in crate::composite::joint) fn outside_at(
        &self,
        model: &JointModel,
        index: usize,
    ) -> Option<(String, String)> {
        match self.way_into(model, index)?.0 {
            super::Move::Reaction(entity, action, _) => Some((entity, action.name.to_string())),
            _ => None,
        }
    }
}

impl JointModel {
    /// Whether `mv` sends an action outside code is relied on to send.
    pub(in crate::composite::joint) fn relies(&self, mv: &super::Move) -> bool {
        mv.outside_action()
            .is_some_and(|(entity, action)| self.relies_on.contains(pair!(entity, action)))
    }
}
