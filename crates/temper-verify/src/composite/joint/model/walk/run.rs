//! Running a walk: its starts, the checker's threads, and what they found.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use dashmap::DashMap;
use stateright::{Checker, Model};

use super::super::super::JointState;
use super::super::super::pack::{Packed, Packer};
use super::super::super::room::{Bound, Held, bytes, grow, map_bytes};
use super::super::JointModel;
use super::checked::Walker;
use super::shared::{Seen, Shared, Trail, lock, read, state_bytes};
use super::{Explored, Keep, Node};

/// The room an expansion is first given; it doubles until what the
/// expansion builds fits.
const BUILD_ROOM: usize = 64 << 10;

impl JointModel {
    /// Walk every reachable joint state, on the checker's threads. One
    /// thread walks breadth first, so each state is first reached by a
    /// shortest path; with more, a state's way there is about as short.
    /// `visit` sees each state as it is taken from the queue, with whether
    /// sure moves alone reach it, and returns true to stop the walk. `keep`
    /// says what else the walk keeps; only with [`Keep::Sure`] is every
    /// state sure moves reach seen as one. The walk takes the memory it
    /// holds from `bound`'s room and stops when the room says no.
    pub(in crate::composite::joint) fn walk_with<'a>(
        self: &Arc<Self>,
        bound: Bound<'a>,
        keep: Keep,
        visit: impl Fn(&JointModel, usize, &JointState, bool) -> bool + Send + Sync + 'static,
    ) -> Explored<'a> {
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
        let threads = bound.room.threads().max(1);
        let seen = DashMap::new();
        let mut states = 0;
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
            let more = grow(&mut explored.nodes, 1, &mut explored.held).and_then(|()| {
                explored.held.set(
                    states
                        + explored.packer.bytes()
                        + bytes(&explored.nodes)
                        + map_bytes::<Packed, Seen>(seen.capacity()),
                )
            });
            if let Err(reason) = more {
                explored.stop_for_room(reason);
                return explored;
            }
            states += state_bytes(&packed);
            let at = explored.nodes.len() as u32;
            seen.insert(packed.clone(), Seen { at, sure: true });
            explored.nodes.push(Node::new(packed, None, true, true));
        }
        let starts = explored.nodes.len() as u32;
        let shared = Arc::new(Shared {
            trail: RwLock::new(Trail {
                nodes: std::mem::take(&mut explored.nodes),
                made: Vec::new(),
                edges: Vec::new(),
                outside: BTreeMap::new(),
                states,
            }),
            packer: RwLock::new(std::mem::take(&mut explored.packer)),
            seen,
            stop: AtomicBool::new(false),
            held_back: AtomicBool::new(false),
            build_room: AtomicUsize::new(BUILD_ROOM),
            wanted: AtomicUsize::new(0),
            waiting: AtomicUsize::new(starts as usize),
            count_at: AtomicUsize::new(0),
            over: AtomicBool::new(false),
            gate: Mutex::new(()),
            due: AtomicBool::new(false),
            asks: Condvar::new(),
            answers: Condvar::new(),
        });
        let refused = Self::run(
            Walker {
                model: Arc::clone(self),
                shared: Arc::clone(&shared),
                keep,
                visit: Box::new(visit),
                starts,
            },
            &shared,
            &mut explored.held,
            threads,
        );
        let shared = Arc::into_inner(shared).expect("the walk's threads have ended");
        let stopped = shared.stop.load(Ordering::Relaxed);
        let trail = shared.trail.into_inner().expect("a walk thread panicked");
        explored.nodes = trail.nodes;
        explored.made = trail.made;
        explored.edges = trail.edges;
        explored.outside = trail.outside;
        explored.packer = shared.packer.into_inner().expect("a walk thread panicked");
        explored.held_back = shared.held_back.load(Ordering::Relaxed);
        let tables = trail.states
            + explored.packer.bytes()
            + bytes(&explored.nodes)
            + bytes(&explored.made)
            + bytes(&explored.edges);
        explored.held.walked(explored.nodes.len());
        if let Some(reason) = refused {
            explored.stop_for_room(reason);
        } else if let Err(reason) = explored.held.set(tables) {
            explored.stop_for_room(reason);
        } else if !stopped && explored.nodes.iter().any(Node::queued) {
            // The checker took a job for one it had: two numbers with one
            // 64-bit hash, once in many billions of walks. Saying so keeps a
            // state it dropped from reading as checked.
            explored.stopped =
                Some("the checker lost a state to a hash collision: walk it again".into());
        }
        if self.expansion.exhausted.get() {
            explored.stopped = Some("the joint expansion exhausted its work or time budget".into());
        }
        explored
    }

    /// Run the checker to its end, this thread counting what the walk holds
    /// for the room while the checker's work. Why the room said no, if it did.
    fn run(walker: Walker, shared: &Shared, held: &mut Held<'_>, threads: usize) -> Option<String> {
        let model = Arc::clone(&walker.model);
        let count =
            |held: &mut Held<'_>, holds| shared.count(held, threads, model.senders.held(), holds);
        if let Err(reason) = count(held, (0, 0)) {
            return Some(reason);
        }
        let checker = walker.checker().threads(threads).spawn_bfs();
        let mut refused = None;
        let mut pause = Duration::from_micros(50);
        while !checker.is_done() {
            let gate = lock(&shared.gate);
            let (gate, _) = shared
                .asks
                .wait_timeout_while(gate, pause, |()| !shared.due.load(Ordering::Acquire))
                .expect("a walk thread panicked");
            pause = (pause * 2).min(Duration::from_millis(10));
            if shared.due.swap(false, Ordering::AcqRel) && refused.is_none() {
                let holds = (checker.unique_state_count(), checker.max_depth());
                if let Err(reason) = count(held, holds) {
                    refused = Some(reason);
                    shared.stop.store(true, Ordering::Release);
                }
                shared.answers.notify_all();
            }
            drop(gate);
            held.walked(read(&shared.trail).nodes.len());
        }
        {
            let _gate = lock(&shared.gate);
            shared.over.store(true, Ordering::Release);
            shared.answers.notify_all();
        }
        drop(checker.join());
        refused
    }
}
