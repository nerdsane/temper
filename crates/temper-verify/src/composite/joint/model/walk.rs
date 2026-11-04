//! What the walk over joint states finds (`walk/run.rs` walks them).

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicU8, Ordering};

use super::super::pack::{Packed, Packer};
use super::super::room::{Held, MB};
use super::super::state::render;
use super::super::{JointState, JointStep};
use super::JointModel;

mod checked;
mod invariants;
mod run;
mod shared;

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

/// A state walked.
pub(super) struct Node {
    pub(super) packed: Packed,
    /// The state and move (a name in `packer`) it was first reached by, or
    /// once sure moves alone reach it, the first sure one.
    pub(super) parent: Option<(u32, u32)>,
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
        parent: Option<(u32, u32)>,
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
    /// The moves (names in `packer`) that are reactions from a type outside
    /// the scope, with the type and action each one sends.
    pub(super) outside: BTreeMap<u32, (String, String)>,
    /// The memory its tables hold, given back when it is dropped.
    pub(in crate::composite::joint) held: Held<'a>,
}

impl Explored<'_> {
    /// Stop the walk: its room said no.
    fn stop_for_room(&mut self, reason: String) {
        self.stopped = Some(format!(
            "the walk does not fit in the memory it may hold: {reason} \
             (it held {} MB at {} states)",
            self.held.bytes / MB,
            self.len()
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
    pub(in crate::composite::joint) fn path(&self, mut index: usize) -> Vec<JointStep> {
        let mut steps = Vec::new();
        loop {
            let parent = self.nodes[index].parent;
            steps.push(JointStep {
                action: parent.map(|(_, label)| self.packer.name(label).to_string()),
                state: render(&self.state(index)),
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
    /// a name in the walk's packer ([`Self::label`]).
    pub(in crate::composite::joint) fn first_unsure(&self, mut index: usize) -> Option<u32> {
        let mut first = None;
        while let Some((from, label)) = self.nodes[index].parent {
            if !self.nodes[index].sure() && self.nodes[from as usize].sure() {
                first = Some(label);
            }
            index = from as usize;
        }
        first
    }

    /// The move a name in the walk's packer stands for, in words.
    pub(in crate::composite::joint) fn label(&self, label: u32) -> &str {
        self.packer.name(label)
    }

    /// The type and action the move `label` sends, when it is a reaction
    /// from a type outside the scope.
    pub(in crate::composite::joint) fn outside_at(&self, label: u32) -> Option<(&str, &str)> {
        self.outside
            .get(&label)
            .map(|(entity, action)| (entity.as_str(), action.as_str()))
    }
}

impl JointModel {
    /// Whether `mv` sends an action outside code is relied on to send.
    pub(in crate::composite::joint) fn relies(&self, mv: &super::Move) -> bool {
        mv.outside_action().is_some_and(|(entity, action)| {
            self.relies_on
                .contains(&(entity.to_string(), action.to_string()))
        })
    }
}
