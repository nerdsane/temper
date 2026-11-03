//! The breadth-first walk over joint states.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

use super::super::env::Blind;
use super::super::pack::{Packed, Packer};
use super::super::room::{Bound, Held, MB, block};
use super::super::state::render;
use super::super::{JointState, JointStep};
use super::JointModel;
use super::held::Tables;
use super::step::{Certainty, Move};

mod invariants;

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
    /// Sure moves alone reach it from a start.
    pub(super) sure: bool,
    /// It waits in the walk's queue.
    pub(super) queued: bool,
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
    /// The moves the runtime could make in a walked state that the walk
    /// never takes.
    pub(super) unwalked: BTreeSet<Unwalked>,
    /// The moves (names in `packer`) that are reactions from a type outside
    /// the scope, with the type and action each one sends.
    pub(super) outside: BTreeMap<u32, (String, String)>,
    /// The memory its tables hold, given back when it is dropped.
    pub(in crate::composite::joint) held: Held<'a>,
}

/// A move the runtime could make that the walk never takes.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::composite::joint) struct Unwalked {
    /// `Entity.Action`.
    pub(in crate::composite::joint) action: String,
    /// Who sends it in the runtime.
    pub(in crate::composite::joint) sender: String,
    /// The record's status where it could.
    pub(in crate::composite::joint) from: String,
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

    /// The first move on the way to the state walked `index`th that the
    /// walk cannot be sure the runtime makes, when there is one: the move
    /// into the first state on it that sure moves alone do not reach. It is
    /// a name in the walk's packer ([`Self::label`]).
    pub(in crate::composite::joint) fn first_unsure(&self, mut index: usize) -> Option<u32> {
        let mut first = None;
        while let Some((from, label)) = self.nodes[index].parent {
            if !self.nodes[index].sure && self.nodes[from as usize].sure {
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

    /// Remember that `label` names `mv`, when it is a reaction from a type
    /// outside the scope.
    fn note_outside(&mut self, label: u32, mv: &Move) {
        if let Move::Reaction(entity, action, _) = mv {
            self.outside
                .entry(label)
                .or_insert_with(|| (entity.clone(), action.name.clone()));
        }
    }

    /// The moves the runtime could make that the walk never took, one line
    /// per sender: `who may take Entity.Action (from Status, ...), ...`.
    pub(in crate::composite::joint) fn unwalked(&self) -> Vec<String> {
        let mut by: BTreeMap<&str, BTreeMap<&str, Vec<&str>>> = BTreeMap::new();
        for found in &self.unwalked {
            by.entry(&found.sender)
                .or_default()
                .entry(&found.action)
                .or_default()
                .push(&found.from);
        }
        by.into_iter()
            .map(|(sender, actions)| {
                let actions: Vec<String> = actions
                    .into_iter()
                    .map(|(action, statuses)| format!("{action} (from {})", statuses.join(", ")))
                    .collect();
                format!("{sender} may take {}", actions.join(", "))
            })
            .collect()
    }
}

/// Who sends an action the walk leaves to a module.
const ANSWER: &str = "a module whose trigger names no on_success";

impl JointModel {
    /// Note each move the runtime could make in `state` that the walk never
    /// takes.
    pub(super) fn note_unwalked(&self, state: &JointState, unwalked: &mut BTreeSet<Unwalked>) {
        for (entity, ways) in &self.unsent {
            let open = state
                .calls
                .iter()
                .any(|call| &call.to == entity && call.on_success.is_empty());
            for index in ways {
                let t = &self.models[entity].transitions[*index];
                if open && self.enabled(entity, t, state, Blind::Nothing) {
                    unwalked.insert(Unwalked {
                        action: format!("{entity}.{}", t.name),
                        sender: ANSWER.to_string(),
                        from: state.records[entity].status.clone(),
                    });
                }
            }
        }
    }
}

/// Why a walk that never took the moves `unwalked` proves nothing.
pub(in crate::composite::joint) fn unwalked_reason(unwalked: &[String]) -> String {
    format!(
        "the walk never takes moves the runtime can make, so it never goes where they lead: {}. Declare an action only a module answers with as internal, or name it in its trigger's on_success",
        unwalked.join("; ")
    )
}

impl JointModel {
    /// Whether `mv` sends an action outside code is relied on to send.
    pub(in crate::composite::joint) fn relies(&self, mv: &super::Move) -> bool {
        mv.outside_action().is_some_and(|(entity, action)| {
            self.relies_on
                .contains(&(entity.to_string(), action.to_string()))
        })
    }

    /// Walk every reachable joint state breadth first, so each state is
    /// first reached by a shortest path. `visit` sees each state as it is
    /// taken from the queue, with whether sure moves alone reach it, and
    /// returns true to stop the walk. `keep` says what else the walk keeps;
    /// only with [`Keep::Sure`] is every state sure moves reach seen as one.
    /// The walk takes the memory it holds from `bound`'s room and stops when
    /// the room says no.
    pub(in crate::composite::joint) fn walk_with<'a>(
        &self,
        bound: Bound<'a>,
        keep: Keep,
        mut visit: impl FnMut(usize, &JointState, bool) -> bool,
    ) -> Explored<'a> {
        let edges = keep == Keep::Edges;
        // The room bounds what the walk holds; a model's expansion work has
        // no count or clock of its own here.
        self.expansion.reset(usize::MAX, None);
        let mut explored = Explored {
            nodes: Vec::new(),
            packer: Packer::default(),
            made: Vec::new(),
            edges: Vec::new(),
            stopped: None,
            held_back: false,
            unwalked: BTreeSet::new(),
            outside: BTreeMap::new(),
            held: Held::new(bound),
        };
        if let Some(reason) = &self.senders.problem {
            explored.stopped = Some(reason.clone());
            return explored;
        }
        // Check the sliced models before either caller can expand a move.
        let oversized: Vec<_> = self
            .models
            .iter()
            .filter_map(|(entity, model)| {
                crate::model::semantics::oversized_transition(model)
                    .map(|(action, _)| format!("{entity}.{action} parameter expansion"))
            })
            .collect();
        if !oversized.is_empty() {
            explored.stopped = Some(format!(
                "unsupported reads or expansion: {}",
                oversized.join(", ")
            ));
            return explored;
        }
        let mut seen: HashMap<Packed, u32> = HashMap::new();
        let mut queue = VecDeque::new();
        let mut tables = Tables::default();
        // One start per way the records' ids can be equal: many records start
        // in millions of ways, so the starts take their memory from the room
        // as the walk's states do.
        for init in self.inits() {
            if !self.expansion.take() {
                break;
            }
            let packed = explored.packer.pack(&init);
            if seen.contains_key(&packed) {
                continue;
            }
            if let Err(reason) = tables.make_room(&mut explored, &mut seen, &mut queue, edges) {
                explored.stop_for_room(reason);
                return explored;
            }
            tables.states += block(size_of::<[usize; 2]>() + 4 * packed.len());
            seen.insert(packed.clone(), explored.nodes.len() as u32);
            queue.push_back(explored.nodes.len());
            explored.nodes.push(Node {
                packed,
                parent: None,
                sure: true,
                queued: true,
            });
        }
        while let Some(index) = queue.pop_front() {
            explored.nodes[index].queued = false;
            let sure = explored.nodes[index].sure;
            let state = explored.state(index);
            self.note_unwalked(&state, &mut explored.unwalked);
            if visit(index, &state, sure) {
                break;
            }
            explored.held.walked(explored.nodes.len());
            if let Err(reason) = tables.make_room(&mut explored, &mut seen, &mut queue, edges) {
                explored.stop_for_room(reason);
                break;
            }
            let before = [
                explored.nodes.len(),
                explored.made.len(),
                explored.edges.len(),
            ];
            let moves = match self.within(&mut explored.held, &mut tables, || self.moves(&state)) {
                Ok(moves) => moves,
                Err(reason) => {
                    explored.stop_for_room(reason);
                    break;
                }
            };
            for mv in &moves.0 {
                if !self.expansion.take() {
                    break;
                }
                let ((nexts, held), room) =
                    match self.within(&mut explored.held, &mut tables, || self.next(&state, mv)) {
                        Ok(built) => built,
                        Err(reason) => {
                            explored.stop_for_room(reason);
                            break;
                        }
                    };
                explored.held_back |= held;
                tables.decided = self.senders.held();
                // Every outcome needs an edge; state slots are reserved only
                // when an unseen packed state is about to be kept.
                let reserved =
                    tables.reserve(&mut explored, &mut seen, &mut queue, edges, 0, nexts.len());
                if let Err(reason) = reserved {
                    tables.give(&mut explored.held, room);
                    explored.stop_for_room(reason);
                    break;
                }
                if edges {
                    explored.made.push(Made {
                        from: index as u32,
                        by_system: mv.by_system(),
                        relied: self.relies(mv),
                        outside: mv.is_outside_reaction(),
                        covered: !held
                            && !nexts.is_empty()
                            && nexts.iter().all(|n| n.certainty != Certainty::Unsure),
                    });
                }
                let made = explored.made.len().saturating_sub(1) as u32;
                for next in nexts {
                    let packed = explored.packer.pack(&next.state);
                    let surely = sure && next.certainty == Certainty::Sure;
                    let at = match seen.get(&packed) {
                        Some(&at)
                            if keep == Keep::Sure
                                && surely
                                && !explored.nodes[at as usize].sure =>
                        {
                            // Sure moves alone reach it now: walk it again
                            // unless it still waits to be walked.
                            if !explored.nodes[at as usize].queued {
                                if let Err(reason) = tables.reserve(
                                    &mut explored,
                                    &mut seen,
                                    &mut queue,
                                    false,
                                    1,
                                    0,
                                ) {
                                    explored.stop_for_room(reason);
                                    break;
                                }
                                queue.push_back(at as usize);
                            }
                            let label = explored.packer.id(&next.label);
                            explored.note_outside(label, mv);
                            let node = &mut explored.nodes[at as usize];
                            node.sure = true;
                            node.queued = true;
                            node.parent = Some((index as u32, label));
                            at
                        }
                        Some(&at) => at,
                        None => {
                            if let Err(reason) =
                                tables.reserve(&mut explored, &mut seen, &mut queue, false, 1, 0)
                            {
                                explored.stop_for_room(reason);
                                break;
                            }
                            let at = explored.nodes.len() as u32;
                            let label = explored.packer.id(&next.label);
                            explored.note_outside(label, mv);
                            tables.states += block(size_of::<[usize; 2]>() + 4 * packed.len());
                            seen.insert(packed.clone(), at);
                            explored.nodes.push(Node {
                                packed,
                                parent: Some((index as u32, label)),
                                sure: surely,
                                queued: true,
                            });
                            queue.push_back(at as usize);
                            at
                        }
                    };
                    if edges {
                        explored.edges.push(Edge {
                            made,
                            to: at,
                            sure: next.certainty == Certainty::Sure,
                        });
                    }
                }
                tables.give(&mut explored.held, room);
                if let Err(reason) = tables.count(&mut explored, &seen, &queue) {
                    explored.stop_for_room(reason);
                    break;
                }
            }
            tables.give(&mut explored.held, moves.1);
            if explored.stopped.is_some() {
                break;
            }
            let after = [
                explored.nodes.len(),
                explored.made.len(),
                explored.edges.len(),
            ];
            tables.saw([0, 1, 2].map(|i| after[i] - before[i]));
        }
        if self.expansion.exhausted.get() {
            explored.stopped = Some("the joint expansion exhausted its work or time budget".into());
        }
        explored
    }
}
