//! The walk over joint states, on Stateright's checker and its threads.
//!
//! The checker's queue and threads drive the walk; what it hands round is a
//! state's number, so its queue stays small and no state is built twice. A
//! state's moves are made in the model's `actions`, where each new state
//! gets its number. The model keeps every state whole in [`Shared::seen`], so
//! what the walk finds is exact, and what Stateright keeps by a 64-bit hash
//! of the number can only ever lose a state, which [`JointModel::walk_with`]
//! notices and says.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use dashmap::mapref::entry::Entry;
use stateright::{Model, Property};
use thread_local::ThreadLocal;

use super::super::super::JointState;
use super::super::super::pack::{Packed, parts};
use super::super::super::state::render;
use super::super::JointModel;
use super::super::moves::{Move, dispatch_move};
use super::levels::deeper;
use super::making::Target;
use super::memo::{Holder, Memo, check_step};
use super::shared::{Seen, Shared, state_bytes, write};
use super::{Edge, Keep, Made, Node, Via};

/// What a walk sees each state with: the model, its number, the state, and
/// whether sure moves alone reach it. True stops the walk.
pub(super) type Visit = Box<dyn Fn(&JointModel, usize, &JointState, bool) -> bool + Send + Sync>;

/// A state to walk, by number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct Walked {
    pub(super) at: u32,
    /// Sure moves alone have come to reach it since it was first walked. It
    /// happens to a state once, so this and the number name one job.
    again: bool,
    /// How many moves from a start it was found.
    pub(super) depth: u16,
}

/// One way a move leads, waiting to be added to the trail.
pub(super) struct Way {
    /// The move, an index into the batch's `made`.
    pub(super) made: u32,
    pub(super) to: To,
    pub(super) sure: bool,
}

pub(super) enum To {
    /// A state the walk has, that this way tells it nothing new about.
    Known(u32),
    /// A state to look for again with the trail held: it may be new, or
    /// sure moves alone may now reach it.
    Check {
        packed: Packed,
        /// The way it is reached.
        via: Via,
        /// Sure moves alone reach it this way.
        surely: bool,
    },
}

/// What walking one state found so far.
#[derive(Default)]
pub(super) struct Batch {
    pub(super) made: Vec<Made>,
    pub(super) ways: Vec<Way>,
}

/// Ways a thread keeps before adding them to the trail, so what it holds
/// aside stays small next to what the walk holds.
pub(super) const BATCH: usize = 4096;

pub(super) struct Walker {
    pub(super) model: Arc<JointModel>,
    pub(super) shared: Arc<Shared>,
    pub(super) keep: Keep,
    pub(super) visit: Visit,
    pub(super) starts: u32,
    /// What each thread remembers of the moves it has made.
    pub(super) memos: ThreadLocal<RefCell<Memo>>,
}

/// A state being walked, and what its moves have found.
pub(super) struct Walking<'a> {
    pub(super) job: Walked,
    pub(super) sure: bool,
    pub(super) state: &'a JointState,
    /// How many moves of the state came before.
    pub(super) by: u32,
    pub(super) batch: Batch,
    /// What the thread remembers.
    pub(super) memo: &'a mut Memo,
    /// The state's moves as remembered, when they are being checked.
    pub(super) checked_moves: Option<Vec<Move>>,
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
        let node = shared.nodes.get(job.at);
        let (packed, sure) = (&node.packed, node.start());
        let mut memo = self
            .memos
            .get_or_default()
            .try_borrow_mut()
            .expect("a thread walks one state at a time");
        let state = memo.mirror.unpack(packed, &shared.packer);
        if (self.visit)(&self.model, index, &state, sure) {
            shared.stop.store(true, Ordering::Relaxed);
            return;
        }
        let mut walking = Walking {
            job,
            sure,
            state: &state,
            by: 0,
            batch: Batch::default(),
            checked_moves: check_step(memo.steps).then(Vec::new),
            memo: &mut memo,
        };
        if self.walk_groups(packed, &mut walking, jobs).is_none() {
            return;
        }
        if let Some(remembered) = &walking.checked_moves {
            let Some(fresh) = self.built(|| self.model.moves(&state)) else {
                return;
            };
            if let Some(stray) = self.model.value_out_of_class(&state) {
                panic!("{stray} in {}", render(&state));
            }
            assert!(
                *remembered == fresh,
                "a state's remembered moves differ from its moves: {remembered:?} and {fresh:?} from {}",
                render(&state)
            );
        }
        let states = self.add(job, &mut walking.batch, jobs);
        if states >= shared.count_at.load(Ordering::Acquire) {
            shared.wait(|| shared.count_at.load(Ordering::Acquire) > states);
        }
    }

    /// Make each of the state's moves: the records', each call's, each
    /// dispatch's, in the order [`JointModel::moves`] names them. None when
    /// the walk ended first.
    fn walk_groups(
        &self,
        packed: &[u32],
        walking: &mut Walking<'_>,
        jobs: &mut Vec<Walked>,
    ) -> Option<()> {
        let model = &*self.model;
        let state = walking.state;
        let (held, flying, calls, armed) = parts(packed, state.records.len());
        let strings = model.values(state);
        let place = walking.memo.place(model, (held, armed), state, &strings);
        if model.asked(state) {
            if walking.memo.move_count(place, Holder::Records).is_none() {
                let moves = self.built(|| {
                    let mut out = Vec::new();
                    model.record_moves(state, &strings, &mut out);
                    out
                })?;
                walking.memo.keep_moves(place, Holder::Records, moves);
            }
            self.walk_moves(place, Holder::Records, walking, jobs)?;
        }
        for (at, &id) in calls.iter().enumerate() {
            if at > 0 && calls[at - 1] == id {
                continue;
            }
            let holder = Holder::Call(id);
            if walking.memo.move_count(place, holder).is_none() {
                let moves = self.built(|| {
                    let mut out = Vec::new();
                    model.call_moves(state, &state.calls[at], &strings, &mut out);
                    out
                })?;
                walking.memo.keep_moves(place, holder, moves);
            }
            self.walk_moves(place, holder, walking, jobs)?;
        }
        for (at, &id) in flying.iter().enumerate() {
            if at > 0 && flying[at - 1] == id {
                continue;
            }
            let dispatch = &state.inflight[at];
            if let Some(checked) = &mut walking.checked_moves {
                checked.push(dispatch_move(dispatch));
            }
            self.walk_one(Target::Dispatch(place, id, dispatch), walking, jobs)?;
        }
        Some(())
    }

    /// Count `bytes` more of what the threads remember.
    pub(super) fn remember(&self, bytes: usize) {
        self.shared.remembered.fetch_add(bytes, Ordering::Relaxed);
    }

    /// Run `build` in the room an expansion may build in, asking the calling
    /// thread for more until it fits. None when the room has no more.
    pub(super) fn built<T>(&self, build: impl Fn() -> T) -> Option<T> {
        let shared = &*self.shared;
        let expansion = &self.model.expansion;
        loop {
            let room = shared.build_room.load(Ordering::Acquire);
            expansion.allow(room);
            expansion.name_moves(false);
            let built = build();
            expansion.name_moves(true);
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
    pub(super) fn add(&self, from: Walked, batch: &mut Batch, jobs: &mut Vec<Walked>) -> usize {
        let shared = &*self.shared;
        let before = jobs.len();
        let mut trail = (self.keep == Keep::Edges).then(|| write(&shared.trail));
        let first_made = trail.as_mut().map_or(0, |trail| {
            let first = trail.made.len() as u32;
            trail.made.append(&mut batch.made);
            first
        });
        let mut new_bytes = 0;
        for way in batch.ways.drain(..) {
            let to = match way.to {
                To::Known(at) => at,
                To::Check {
                    packed,
                    via,
                    surely,
                } => self.check(packed, from, via, surely, jobs, &mut new_bytes),
            };
            if let Some(trail) = &mut trail {
                trail.edges.push(Edge {
                    made: first_made + way.made,
                    to,
                    sure: way.sure,
                });
            }
        }
        drop(trail);
        if new_bytes > 0 {
            shared.states.fetch_add(new_bytes, Ordering::Relaxed);
        }
        let made = jobs.len() - before;
        shared.levels.made(deeper(from.depth), made);
        shared.waiting.fetch_add(made, Ordering::Relaxed);
        shared.nodes.len()
    }

    /// The number of the state `packed`, which `via` leads to from the state
    /// `from`: a new one when the walk does not have it. A job follows when
    /// it is new, or when sure moves alone now reach it and it does not wait
    /// to be walked.
    fn check(
        &self,
        packed: Packed,
        from: Walked,
        via: Via,
        surely: bool,
        jobs: &mut Vec<Walked>,
        new_bytes: &mut usize,
    ) -> u32 {
        let parent = (from.at, via);
        let depth = deeper(from.depth);
        match self.shared.seen.entry(packed.clone()) {
            Entry::Occupied(mut found) => {
                let at = found.get().at;
                self.shared.levels.found_again(found.get().depth, depth);
                if self.keep == Keep::Sure && surely && !found.get().sure {
                    found.get_mut().sure = true;
                    self.shared.nodes.change_way(at, parent);
                    if self.shared.nodes.get(at).found_sure() {
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
                *new_bytes += state_bytes(&packed);
                let at = self
                    .shared
                    .nodes
                    .add(Node::new(packed, Some(parent), surely, true));
                free.insert(Seen {
                    at,
                    sure: surely,
                    depth,
                });
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
