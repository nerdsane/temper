//! The memory a joint walk may hold.
//!
//! Anyone who may install an app can send specs whose joint model never
//! ends. What such a walk uses up is the memory of the host running it: past
//! that, the host kills the process and every tenant on it. So a walk asks a
//! [`Room`] for memory as it grows, and stops when the room says no. Time is
//! not bounded: a walk that fits finishes, since each state it walks is one
//! more it holds.

mod construction;
pub(super) use construction::Construction;

use std::collections::{HashMap, VecDeque};
use std::hash::Hash;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Memory joint walks take as they grow and give back when they end.
pub trait Room: Sync {
    /// Take `bytes` more, or say why not.
    fn take(&self, bytes: usize) -> Result<(), String>;
    /// Give back `bytes` taken before.
    fn give(&self, bytes: usize);
    /// How many threads a walk may use.
    fn threads(&self) -> usize {
        1
    }
}

/// A fixed amount of memory, for tools and tests.
#[derive(Debug)]
pub struct Bytes {
    limit: usize,
    held: AtomicUsize,
    threads: usize,
}

impl Bytes {
    /// A room of `limit` bytes.
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            held: AtomicUsize::new(0),
            threads: 1,
        }
    }

    /// The same room, walked by `threads` threads.
    pub fn with_threads(self, threads: usize) -> Self {
        Self { threads, ..self }
    }

    /// The bytes taken and not given back.
    pub fn held(&self) -> usize {
        self.held.load(Ordering::Relaxed)
    }
}

impl Room for Bytes {
    fn take(&self, bytes: usize) -> Result<(), String> {
        let mut held = self.held.load(Ordering::Relaxed);
        loop {
            let after = held.saturating_add(bytes);
            if after > self.limit {
                return Err(format!("it needs more than its {} MB", self.limit / MB));
            }
            match self
                .held
                .compare_exchange(held, after, Ordering::Relaxed, Ordering::Relaxed)
            {
                Ok(_) => return Ok(()),
                Err(now) => held = now,
            }
        }
    }

    fn give(&self, bytes: usize) {
        self.held.fetch_sub(bytes, Ordering::Relaxed);
    }

    fn threads(&self) -> usize {
        self.threads
    }
}

/// How far a walk has got, read while it runs.
#[derive(Debug, Default)]
pub struct Progress {
    /// Joint states walked so far.
    pub states: AtomicUsize,
    /// Bytes the walk holds.
    pub bytes: AtomicUsize,
    /// How many moves from a start the walk has covered: it has walked every
    /// state that near.
    pub depth: AtomicUsize,
    /// How many moves the walk is working at. Threads can find a state late,
    /// so this says how far it has got, and `depth` what is proved.
    pub reached: AtomicUsize,
}

/// Where a joint check takes its memory, and where it says how far it got.
#[derive(Clone, Copy)]
pub struct Bound<'a> {
    pub room: &'a dyn Room,
    pub progress: &'a Progress,
}

pub(super) const MB: usize = 1 << 20;

/// What a walk holds, counted as it grows, and the memory it took for it.
pub(super) struct Held<'a> {
    bound: Bound<'a>,
    /// Bytes the walk's tables hold.
    pub(super) bytes: usize,
    /// Bytes taken from the room: never less than `bytes`.
    taken: usize,
}

impl<'a> Held<'a> {
    pub(super) fn new(bound: Bound<'a>) -> Self {
        Self {
            bound,
            bytes: 0,
            taken: 0,
        }
    }

    /// Hold `bytes` more, taking memory from the room in steps of an eighth
    /// of what is held, so the room is asked a few dozen times per walk.
    pub(super) fn add(&mut self, bytes: usize) -> Result<(), String> {
        let needed = self.bytes + bytes;
        if needed > self.taken {
            let step = (needed - self.taken).max(self.taken / 8);
            if self.bound.room.take(step).is_err() {
                self.bound.room.take(needed - self.taken)?;
                self.taken = needed;
            } else {
                self.taken += step;
            }
        }
        self.bytes = needed;
        self.bound
            .progress
            .bytes
            .store(self.bytes, Ordering::Relaxed);
        Ok(())
    }

    /// Hold `bytes` fewer.
    pub(super) fn sub(&mut self, bytes: usize) {
        self.bytes -= bytes;
    }

    /// No longer hold up to `bytes` of what was counted: a walk the room
    /// refused may have grown past its last count.
    pub(super) fn give_back(&mut self, bytes: usize) {
        self.bytes -= bytes.min(self.bytes);
    }

    /// Give the room back what it took beyond what is held now.
    pub(super) fn release(&mut self) {
        self.bound.room.give(self.taken - self.bytes);
        self.taken = self.bytes;
    }

    /// Hold `bytes` in all, counted afresh.
    pub(super) fn set(&mut self, bytes: usize) -> Result<(), String> {
        match bytes.checked_sub(self.bytes) {
            Some(more) => self.add(more),
            None => {
                self.sub(self.bytes - bytes);
                Ok(())
            }
        }
    }

    /// Say how many states the walk has.
    pub(super) fn walked(&self, states: usize) {
        self.bound.progress.states.store(states, Ordering::Relaxed);
    }

    /// Say how many moves the walk is working at.
    pub(super) fn reached(&self, depth: usize) {
        self.bound.progress.reached.store(depth, Ordering::Relaxed);
    }

    /// Say how many moves from a start the walk has covered.
    pub(super) fn covered(&self, depth: usize) {
        self.bound.progress.depth.store(depth, Ordering::Relaxed);
    }
}

impl Drop for Held<'_> {
    fn drop(&mut self) {
        self.bound.room.give(self.taken);
    }
}

pub(super) use crate::model::heap::{Heap, block};

/// The bytes a `HashMap<K, V>` that holds `capacity` entries takes: a
/// bucket and a control byte for each, at most seven of eight buckets full.
pub(super) fn map_bytes<K, V>(capacity: usize) -> usize {
    (capacity * 8).div_ceil(7) * (size_of::<(K, V)>() + 1)
}

/// A list a walk keeps.
pub(super) trait List {
    /// Bytes one entry takes.
    const ENTRY: usize;
    fn len(&self) -> usize;
    fn capacity(&self) -> usize;
    fn reserve_exact(&mut self, more: usize);
}

impl<T> List for Vec<T> {
    const ENTRY: usize = size_of::<T>();
    fn len(&self) -> usize {
        Vec::len(self)
    }
    fn capacity(&self) -> usize {
        Vec::capacity(self)
    }
    fn reserve_exact(&mut self, more: usize) {
        Vec::reserve_exact(self, more);
    }
}

impl<T> List for VecDeque<T> {
    const ENTRY: usize = size_of::<T>();
    fn len(&self) -> usize {
        VecDeque::len(self)
    }
    fn capacity(&self) -> usize {
        VecDeque::capacity(self)
    }
    fn reserve_exact(&mut self, more: usize) {
        VecDeque::reserve_exact(self, more);
    }
}

/// The bytes `list` takes at its capacity.
pub(super) fn bytes<L: List>(list: &L) -> usize {
    list.capacity() * L::ENTRY
}

/// Make `list` hold `spare` more, doubling it when it must grow, with the
/// memory of its new copy taken while the old one is still held.
pub(super) fn grow<L: List>(list: &mut L, spare: usize, held: &mut Held<'_>) -> Result<(), String> {
    let len = list.len();
    if list.capacity() - len >= spare {
        return Ok(());
    }
    let wanted = (len + spare).max(list.capacity() * 2);
    held.add(wanted * L::ENTRY)?;
    let old = bytes(list);
    list.reserve_exact(wanted - len);
    held.sub(old);
    Ok(())
}

/// Make `map` hold `spare` more, as [`grow`] does a list.
pub(super) fn grow_map<K: Eq + Hash, V>(
    map: &mut HashMap<K, V>,
    spare: usize,
    held: &mut Held<'_>,
) -> Result<(), String> {
    let len = map.len();
    if map.capacity() - len >= spare {
        return Ok(());
    }
    let wanted = (len + spare).max(map.capacity() * 2);
    // Hash tables round their bucket count up to a power of two, leaving
    // one bucket empty for small tables and an eighth empty thereafter.
    let buckets = (wanted * 8).div_ceil(7).next_power_of_two().max(4);
    let capacity = if buckets < 8 {
        buckets - 1
    } else {
        buckets / 8 * 7
    };
    held.add(map_bytes::<K, V>(capacity))?;
    let old = map_bytes::<K, V>(map.capacity());
    map.reserve(wanted - len);
    held.sub(old);
    Ok(())
}

impl Heap for super::ReactionStep {
    fn heap(&self) -> usize {
        self.entity_type.heap() + self.action.heap() + self.generated_for.heap()
    }
}

impl Heap for super::Dispatch {
    fn heap(&self) -> usize {
        self.to.heap()
            + self.action.heap()
            + self.target_state.heap()
            + self.params.heap()
            + self.chain.heap()
            + self.caller.as_ref().map_or(0, Heap::heap)
            + self
                .unread_guard
                .as_ref()
                .map_or(0, |c| c.status.heap() + c.fields.heap() + c.params.heap())
    }
}

impl Heap for super::Call {
    fn heap(&self) -> usize {
        self.to.heap()
            + self.module.heap()
            + self.on_success.heap()
            + self.on_failure.heap()
            + self.asked.heap()
            + self.caller.as_ref().map_or(0, Heap::heap)
    }
}

impl Heap for super::JointState {
    fn heap(&self) -> usize {
        self.records.heap() + self.inflight.heap() + self.calls.heap() + self.armed.heap()
    }
}
