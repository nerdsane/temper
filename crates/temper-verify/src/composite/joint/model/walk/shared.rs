//! What a walk's threads share, and how the calling thread counts it.
//!
//! The room is asked by the calling thread only, which counts what the walk
//! holds while the threads work. Each thread waits for it when the walk has
//! grown a step ([`next_count`]), so what a walk holds past its room is at
//! most a step, and one thread stops where it stopped the last time: the same
//! specs in the same room give the same walk.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard};

use dashmap::DashMap;

use super::super::super::pack::{Packed, Packer};
use super::super::super::room::{Held, List, MB, block, bytes, grow, map_bytes};
use super::levels::Levels;
use super::{Edge, Made, Node};

/// What Stateright's checker holds, as a walk counts it: an entry in its
/// map of 64-bit hashes for each job it was given, about half full.
const CHECKER_PER_STATE: usize = 48;

/// And for each job waiting: the job, with room in its queue for a second,
/// and besides that the moves to it, a word for each.
const CHECKER_PER_JOB: usize = 2 * 96;

/// A state the walk has, as those looking it up need it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Seen {
    pub(super) at: u32,
    pub(super) sure: bool,
    /// How many moves from a start it was first found.
    pub(super) depth: u16,
}

/// What the threads add to as they find it, in one go for each state.
pub(super) struct Trail {
    pub(super) nodes: Vec<Node>,
    pub(super) made: Vec<Made>,
    pub(super) edges: Vec<Edge>,
    pub(super) outside: BTreeMap<u32, (String, String)>,
    /// Bytes the packed states take.
    pub(super) states: usize,
}

/// What the threads share. A new state is added to `seen` only with `trail`
/// held, so the numbers run in order with no gaps.
pub(super) struct Shared {
    pub(super) trail: RwLock<Trail>,
    pub(super) packer: RwLock<Packer>,
    pub(super) seen: DashMap<Packed, Seen>,
    /// The walk is over: a visit said so, or the room said no.
    pub(super) stop: AtomicBool,
    pub(super) held_back: AtomicBool,
    /// The bytes each thread's expansion may build, as the room allows.
    pub(super) build_room: AtomicUsize,
    /// The bytes an expansion that did not fit asks for.
    pub(super) wanted: AtomicUsize,
    /// Jobs given to the checker and not yet taken.
    pub(super) waiting: AtomicUsize,
    /// What the walk has covered, by how many moves from a start.
    pub(super) levels: Levels,
    /// How many states the walk may have before it is counted again.
    pub(super) count_at: AtomicUsize,
    /// The checker has ended, or is about to: nothing is left to wait for.
    pub(super) over: AtomicBool,
    /// Held to wait for the counting thread and to answer.
    pub(super) gate: Mutex<()>,
    /// A thread waits to be counted.
    pub(super) due: AtomicBool,
    pub(super) asks: Condvar,
    pub(super) answers: Condvar,
}

pub(super) fn read<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().expect("a walk thread panicked")
}

pub(super) fn write<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().expect("a walk thread panicked")
}

pub(super) fn lock(gate: &Mutex<()>) -> MutexGuard<'_, ()> {
    gate.lock().expect("a walk thread panicked")
}

/// How many states the walk may have before it is counted again, when it has
/// `states`: often enough that it never gets far past its room, and seldom
/// enough that the threads hardly wait.
fn next_count(states: usize) -> usize {
    states + (states / 16).clamp(1 << 8, 1 << 16)
}

/// The bytes a packed state takes.
pub(super) fn state_bytes(packed: &Packed) -> usize {
    block(size_of::<[usize; 2]>() + 4 * packed.len())
}

/// Whether `list`, past the size where its next copy matters to the room,
/// nears its end: the threads would make that copy without asking.
fn nears_its_end<L: List>(list: &L) -> bool {
    let capacity = list.capacity();
    capacity * L::ENTRY >= MB && capacity - list.len() < capacity / 8
}

/// Make `list` hold more before it nears its end.
fn stay_ahead<L: List>(list: &mut L, held: &mut Held<'_>) -> Result<(), String> {
    if nears_its_end(list) {
        grow(list, list.capacity() / 8 + 1, held)?;
    }
    Ok(())
}

impl Shared {
    /// Wait for the counting thread to bring about `done`, or for the walk
    /// to end.
    pub(super) fn wait(&self, done: impl Fn() -> bool) {
        let mut gate = lock(&self.gate);
        self.due.store(true, Ordering::Release);
        self.asks.notify_all();
        while !done() && !self.stop.load(Ordering::Acquire) && !self.over.load(Ordering::Acquire) {
            gate = self.answers.wait(gate).expect("a walk thread panicked");
        }
    }

    /// Count afresh what the walk holds, with what the checker `holds`, and
    /// give the threads more room when one asked for it. Called by the one
    /// thread that may ask the room.
    pub(super) fn count(
        &self,
        held: &mut Held<'_>,
        threads: usize,
        decided: usize,
        checker: (usize, usize),
    ) -> Result<(), String> {
        let wanted = self.wanted.load(Ordering::Acquire);
        let room = self.build_room.load(Ordering::Acquire);
        if wanted > room {
            held.add(2 * threads * (wanted - room))
                .map_err(|reason| format!("{reason} for what one move builds"))?;
            self.build_room.store(wanted, Ordering::Release);
        }
        let behind = {
            let trail = read(&self.trail);
            nears_its_end(&trail.nodes) || nears_its_end(&trail.made) || nears_its_end(&trail.edges)
        };
        if behind {
            let mut trail = write(&self.trail);
            stay_ahead(&mut trail.nodes, held)?;
            stay_ahead(&mut trail.made, held)?;
            stay_ahead(&mut trail.edges, held)?;
        }
        let (states, depth) = checker;
        let job = CHECKER_PER_JOB + block(depth.max(1) * size_of::<usize>());
        let waiting = self.waiting.load(Ordering::Relaxed);
        let building = 2 * threads * self.build_room.load(Ordering::Acquire);
        let tables = self.tables();
        held.set(tables + building + decided + states * CHECKER_PER_STATE + waiting * job)?;
        let states = read(&self.trail).nodes.len();
        self.count_at
            .fetch_max(next_count(states), Ordering::AcqRel);
        held.covered(self.levels.covered());
        held.reached(self.levels.reached());
        Ok(())
    }

    /// The bytes the tables take that stay after the walk, and `seen`.
    fn tables(&self) -> usize {
        let trail = read(&self.trail);
        trail.states
            + read(&self.packer).bytes()
            + bytes(&trail.nodes)
            + bytes(&trail.made)
            + bytes(&trail.edges)
            + map_bytes::<Packed, Seen>(self.seen.capacity())
    }
}
