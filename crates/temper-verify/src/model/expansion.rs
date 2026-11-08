//! Work and memory accounting inside state expansion.

use super::heap::{Heap, block};

/// Shared work budget for expansion, including intermediate products. A walk
/// must report incomplete if this budget stops even when its queue is empty.
pub(crate) struct Expansion {
    limit: Option<super::types::ExploreLimit>,
    remaining: std::cell::Cell<usize>,
    until: std::cell::Cell<Option<std::time::Instant>>,
    pub(crate) exhausted: std::cell::Cell<bool>,
    /// Bytes what this expansion builds may still take: a joint walk gives
    /// each expansion room taken from its own, so what one state's moves
    /// build counts against the walk's memory.
    room: std::cell::Cell<usize>,
    /// The expansion needed more than `room` and stopped short.
    short: std::cell::Cell<bool>,
    /// Moves are named in words. A walk turns this off while it expands
    /// states, since nearly every move it makes leads somewhere it has been.
    words: std::cell::Cell<bool>,
    /// What was built read what is in flight, beyond what a walk remembers a
    /// move by, so it holds for this state alone.
    read_flight: std::cell::Cell<bool>,
}
/// Only the name: the allowance is a walk's own accounting, not part of the
/// model, so a model's digest (`JointProof::digest`) leaves it out.
impl std::fmt::Debug for Expansion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Expansion")
    }
}
impl Default for Expansion {
    fn default() -> Self {
        Self {
            limit: None,
            remaining: std::cell::Cell::new(usize::MAX),
            until: std::cell::Cell::new(None),
            exhausted: std::cell::Cell::new(false),
            room: std::cell::Cell::new(usize::MAX),
            short: std::cell::Cell::new(false),
            words: std::cell::Cell::new(true),
            read_flight: std::cell::Cell::new(false),
        }
    }
}
impl Expansion {
    pub(crate) fn for_model(model: &super::types::TemperModel) -> Self {
        Self {
            limit: model.limit.clone(),
            ..Self::default()
        }
    }

    pub(crate) fn reset(&self, states: usize, time: Option<std::time::Duration>) {
        self.remaining.set(states);
        self.until
            .set(time.and_then(|t| std::time::Instant::now().checked_add(t)));
        self.exhausted.set(false);
        self.allow(usize::MAX);
    }

    /// Whether moves are named in words on this thread.
    pub(crate) fn words(&self) -> bool {
        self.words.get()
    }

    /// Name moves in words on this thread, or not.
    pub(crate) fn name_moves(&self, words: bool) {
        self.words.set(words);
    }

    /// What is being built read what is in flight.
    pub(crate) fn note_flight_read(&self) {
        self.read_flight.set(true);
    }

    /// Whether what was built since the last asking read what is in flight.
    pub(crate) fn flight_was_read(&self) -> bool {
        self.read_flight.replace(false)
    }

    /// Run `build` with no bound and moves named in words, leaving the
    /// expansion's own count, clock and room as they were: naming a move a
    /// walk already made is no work of the walk's.
    pub(crate) fn outside_the_count<T>(&self, build: impl FnOnce() -> T) -> T {
        let remaining = self.remaining.replace(usize::MAX);
        let until = self.until.replace(None);
        let exhausted = self.exhausted.replace(false);
        let room = self.room.replace(usize::MAX);
        let short = self.short.replace(false);
        let words = self.words.replace(true);
        let built = build();
        self.remaining.set(remaining);
        self.until.set(until);
        self.exhausted.set(exhausted);
        self.room.set(room);
        self.short.set(short);
        self.words.set(words);
        built
    }

    /// Let what the expansion builds from now on take `bytes`.
    pub(crate) fn allow(&self, bytes: usize) {
        self.room.set(bytes);
        self.short.set(false);
    }

    /// Count `bytes` more of what the expansion builds, or stop it short.
    pub(crate) fn hold(&self, bytes: usize) -> bool {
        match self.room.get().checked_sub(bytes) {
            Some(left) if !self.short.get() => {
                self.room.set(left);
                true
            }
            _ => {
                self.short.set(true);
                false
            }
        }
    }

    /// Reserve a vector slot before allocating its replacement block. Both
    /// blocks count until reserve completes; the old block is then released.
    pub(crate) fn reserve<T>(&self, out: &mut Vec<T>) -> bool {
        if out.len() < out.capacity() {
            return !self.short();
        }
        let capacity = out.capacity().saturating_mul(2).max(1);
        let Some(size) = capacity.checked_mul(size_of::<T>()) else {
            self.short.set(true);
            return false;
        };
        if !self.hold(block(size)) {
            return false;
        }
        let old = out.capacity() * size_of::<T>();
        out.reserve_exact(capacity - out.len());
        if old != 0 {
            self.give(block(old));
        }
        true
    }

    /// Charge a copied list and its added entry before copying it.
    pub(crate) fn with_entry<V: Clone + Heap>(
        &self,
        map: &super::fields::Fields<V>,
        key: impl Into<super::fields::Sym>,
        value: &V,
    ) -> Option<super::fields::Fields<V>> {
        let bytes = block(
            (map.len() + 1) * (size_of::<super::fields::Sym>() + size_of::<V>())
                + 4 * size_of::<usize>(),
        );
        if !self.hold(bytes + value.heap()) {
            return None;
        }
        let mut next = map.clone();
        next.insert(key, value.clone());
        Some(next)
    }

    /// Keep one value, including spare container capacity and growth overlap.
    pub(crate) fn push<T: Heap>(&self, out: &mut Vec<T>, value: T) -> bool {
        if !self.hold(value.heap()) || !self.reserve(out) {
            return false;
        }
        out.push(value);
        true
    }

    /// Count `bytes` fewer: what it built and dropped.
    pub(crate) fn give(&self, bytes: usize) {
        self.room.set(self.room.get().saturating_add(bytes));
    }

    /// Whether the expansion stopped short of the room it was allowed.
    pub(crate) fn short(&self) -> bool {
        self.short.get()
    }

    pub(crate) fn take(&self) -> bool {
        use std::sync::atomic::Ordering;
        if self.short.get() {
            return false;
        }
        if let Some(limit) = &self.limit
            && !limit.lifted.load(Ordering::Relaxed)
            && (limit.past_time()
                || limit
                    .work
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1))
                    .is_err())
        {
            limit.exhausted.store(true, Ordering::Relaxed);
            self.exhausted.set(true);
            return false;
        }
        if self.remaining.get() == 0
            || self
                .until
                .get()
                .is_some_and(|t| std::time::Instant::now() >= t)
        {
            self.exhausted.set(true);
            return false;
        }
        self.remaining.set(self.remaining.get() - 1);
        true
    }
}
