//! The walk over joint states, on Stateright's checker and its threads.
//!
//! The checker's queue and threads drive the walk; what it hands round is a
//! state's number, so its queue stays small and no state is built twice. A
//! state's moves are made in the model's `actions`, where each new state
//! gets its number. The model keeps every state whole in [`Shared::seen`], so
//! what the walk finds is exact, and what Stateright keeps by a 64-bit hash
//! of the number can only ever lose a state, which [`JointModel::walk_with`]
//! notices and says.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use dashmap::mapref::entry::Entry;
use stateright::{Model, Property};

use super::super::super::JointState;
use super::super::super::pack::Packed;
use super::super::JointModel;
use super::super::step::{Certainty, Move};
use super::levels::deeper;
use super::shared::{Seen, Shared, Trail, read, state_bytes, write};
use super::{Edge, Keep, Made, Node};

/// What a walk sees each state with: the model, its number, the state, and
/// whether sure moves alone reach it. True stops the walk.
pub(super) type Visit = Box<dyn Fn(&JointModel, usize, &JointState, bool) -> bool + Send + Sync>;

/// A state to walk, by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Walked {
    at: u32,
    /// Sure moves alone have come to reach it since it was first walked. It
    /// happens to a state once, so this and the number name one job.
    again: bool,
    /// How many moves from a start it was found.
    depth: u16,
}

/// One way a move leads, waiting to be added to the trail.
struct Way {
    /// The move, an index into the batch's `made`.
    made: u32,
    to: To,
    sure: bool,
}

enum To {
    /// A state the walk has, that this way tells it nothing new about.
    Known(u32),
    /// A state to look for again with the trail held: it may be new, or
    /// sure moves alone may now reach it.
    Check {
        packed: Packed,
        /// The move, a name in the packer.
        label: u32,
        /// Sure moves alone reach it this way.
        surely: bool,
        /// The move, an index into the state's moves.
        by: u32,
    },
}

/// What walking one state found so far.
#[derive(Default)]
struct Batch {
    made: Vec<Made>,
    ways: Vec<Way>,
}

/// Ways a thread keeps before adding them to the trail, so what it holds
/// aside stays small next to what the walk holds.
const BATCH: usize = 4096;

pub(super) struct Walker {
    pub(super) model: Arc<JointModel>,
    pub(super) shared: Arc<Shared>,
    pub(super) keep: Keep,
    pub(super) visit: Visit,
    pub(super) starts: u32,
}

impl Model for Walker {
    type State = Walked;
    type Action = Walked;

    fn init_states(&self) -> Vec<Walked> {
        (0..self.starts)
            .map(|at| Walked {
                at,
                again: false,
                depth: 0,
            })
            .collect()
    }

    fn actions(&self, state: &Walked, actions: &mut Vec<Walked>) {
        self.walk(*state, actions);
        // A walk that stopped may have left this one half done.
        let ended =
            self.shared.stop.load(Ordering::Acquire) || self.shared.over.load(Ordering::Acquire);
        if !ended {
            self.shared.levels.finished(state.depth);
        }
    }

    fn next_state(&self, _: &Walked, action: Walked) -> Option<Walked> {
        Some(action)
    }

    /// The checker expands states only while a property waits on them, and
    /// ends once the one here is found false.
    fn properties(&self) -> Vec<Property<Self>> {
        vec![Property::always("the walk goes on", |walker, _| {
            !walker.shared.stop.load(Ordering::Relaxed)
        })]
    }
}

impl Walker {
    /// Walk one state, leaving the jobs it makes in `jobs`.
    fn walk(&self, job: Walked, jobs: &mut Vec<Walked>) {
        let shared = &*self.shared;
        shared.waiting.fetch_sub(1, Ordering::Relaxed);
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let index = job.at as usize;
        let (packed, sure) = {
            let trail = read(&shared.trail);
            let node = &trail.nodes[index];
            (node.packed.clone(), node.start())
        };
        let state = read(&shared.packer).unpack(&packed);
        if (self.visit)(&self.model, index, &state, sure) {
            shared.stop.store(true, Ordering::Relaxed);
            return;
        }
        let Some(moves) = self.built(|| self.model.moves(&state)) else {
            return;
        };
        let edges = self.keep == Keep::Edges;
        let mut batch = Batch::default();
        for (by, mv) in moves.iter().enumerate() {
            if !self.model.expansion.take() {
                break;
            }
            let Some((nexts, held)) = self.built(|| self.model.next(&state, mv)) else {
                return;
            };
            if held {
                shared.held_back.store(true, Ordering::Relaxed);
            }
            if edges {
                batch.made.push(Made {
                    from: job.at,
                    by_system: mv.by_system(),
                    relied: self.model.relies(mv),
                    outside: mv.is_outside_reaction(),
                    covered: !held
                        && !nexts.is_empty()
                        && nexts.iter().all(|n| n.certainty != Certainty::Unsure),
                    held,
                });
            }
            let made = batch.made.len().saturating_sub(1) as u32;
            let depth = deeper(job.depth);
            for next in &nexts {
                let packed = self.pack(&next.state);
                let surely = sure && next.certainty == Certainty::Sure;
                let found = shared.seen.get(&packed).map(|seen| *seen);
                if let Some(seen) = found {
                    shared.levels.found_again(seen.depth, depth);
                }
                let to = match found {
                    Some(seen) if self.keep != Keep::Sure || !surely || seen.sure => {
                        if !edges {
                            continue;
                        }
                        To::Known(seen.at)
                    }
                    _ => To::Check {
                        packed,
                        label: self.label(&next.label),
                        surely,
                        by: by as u32,
                    },
                };
                batch.ways.push(Way {
                    made,
                    to,
                    sure: next.certainty == Certainty::Sure,
                });
            }
            if batch.ways.len() >= BATCH {
                self.add(job, &mut batch, &moves, jobs);
            }
        }
        let states = self.add(job, &mut batch, &moves, jobs);
        if states >= shared.count_at.load(Ordering::Acquire) {
            shared.wait(|| shared.count_at.load(Ordering::Acquire) > states);
        }
    }

    /// `state` packed, adding what the tables do not hold yet.
    fn pack(&self, state: &JointState) -> Packed {
        let known = read(&self.shared.packer).known(state);
        known.unwrap_or_else(|| write(&self.shared.packer).pack(state))
    }

    /// The name of a move in the packer.
    fn label(&self, label: &str) -> u32 {
        let known = read(&self.shared.packer).known_id(label);
        known.unwrap_or_else(|| write(&self.shared.packer).id(label))
    }

    /// Run `build` in the room an expansion may build in, asking the calling
    /// thread for more until it fits. None when the room has no more.
    fn built<T>(&self, build: impl Fn() -> T) -> Option<T> {
        let shared = &*self.shared;
        let expansion = &self.model.expansion;
        loop {
            let room = shared.build_room.load(Ordering::Acquire);
            expansion.allow(room);
            let built = build();
            let short = expansion.short();
            expansion.allow(usize::MAX);
            if !short {
                return Some(built);
            }
            drop(built);
            shared
                .wanted
                .fetch_max(room.saturating_mul(2), Ordering::AcqRel);
            shared.wait(|| shared.build_room.load(Ordering::Acquire) != room);
            if shared.stop.load(Ordering::Relaxed) || shared.over.load(Ordering::Relaxed) {
                return None;
            }
        }
    }

    /// Add what walking the state `from` found to the trail, and say how many
    /// states the walk has.
    fn add(
        &self,
        from: Walked,
        batch: &mut Batch,
        moves: &[Move],
        jobs: &mut Vec<Walked>,
    ) -> usize {
        let shared = &*self.shared;
        let before = jobs.len();
        let states = {
            let mut trail = write(&shared.trail);
            let trail = &mut *trail;
            let first_made = trail.made.len() as u32;
            trail.made.append(&mut batch.made);
            for way in batch.ways.drain(..) {
                let to = match way.to {
                    To::Known(at) => at,
                    To::Check {
                        packed,
                        label,
                        surely,
                        by,
                    } => self.check(
                        trail,
                        packed,
                        from,
                        label,
                        surely,
                        &moves[by as usize],
                        jobs,
                    ),
                };
                if self.keep == Keep::Edges {
                    trail.edges.push(Edge {
                        made: first_made + way.made,
                        to,
                        sure: way.sure,
                    });
                }
            }
            trail.nodes.len()
        };
        let made = jobs.len() - before;
        shared.levels.made(deeper(from.depth), made);
        shared.waiting.fetch_add(made, Ordering::Relaxed);
        states
    }

    /// The number of the state `packed`, which `mv` (named `label`) leads to
    /// from the state `from`: a new one when the walk does not have it. A job
    /// follows when it is new, or when sure moves alone now reach it and it
    /// does not wait to be walked.
    #[allow(clippy::too_many_arguments)]
    fn check(
        &self,
        trail: &mut Trail,
        packed: Packed,
        from: Walked,
        label: u32,
        surely: bool,
        mv: &Move,
        jobs: &mut Vec<Walked>,
    ) -> u32 {
        let parent = (from.at, label);
        let depth = deeper(from.depth);
        match self.shared.seen.entry(packed.clone()) {
            Entry::Occupied(mut found) => {
                let at = found.get().at;
                self.shared.levels.found_again(found.get().depth, depth);
                if self.keep == Keep::Sure && surely && !found.get().sure {
                    found.get_mut().sure = true;
                    note_outside(&mut trail.outside, parent.1, mv);
                    let node = &mut trail.nodes[at as usize];
                    node.parent = Some(parent);
                    if node.found_sure() {
                        jobs.push(Walked {
                            at,
                            again: true,
                            depth,
                        });
                    }
                }
                at
            }
            Entry::Vacant(free) => {
                let at = u32::try_from(trail.nodes.len()).expect("fewer than 2^32 states");
                note_outside(&mut trail.outside, parent.1, mv);
                trail.states += state_bytes(&packed);
                free.insert(Seen {
                    at,
                    sure: surely,
                    depth,
                });
                trail
                    .nodes
                    .push(Node::new(packed, Some(parent), surely, true));
                jobs.push(Walked {
                    at,
                    again: false,
                    depth,
                });
                at
            }
        }
    }
}

/// Remember that `label` names `mv`, when it is a reaction from a type
/// outside the scope.
fn note_outside(outside: &mut BTreeMap<u32, (String, String)>, label: u32, mv: &Move) {
    if let Move::Reaction(entity, action, _) = mv {
        outside
            .entry(label)
            .or_insert_with(|| (entity.clone(), action.name.clone()));
    }
}
