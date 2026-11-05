//! What a walk's threads remember of the moves they make.
//!
//! A walk's states differ mostly in what is in flight, and a move does the same
//! wherever the records, timers, values and sequence numbers are the same: in
//! one walk nearly every move has been made before. So each thread numbers the
//! places it has been, by what a move reads there, and keeps what each move
//! changes. A place holds only numbers, into lists that hold each set of
//! records and each change to what is in flight once: there are many times
//! more moves remembered than there are of those. What depends on what is in
//! flight as a whole is worked out for each state ([`JointModel::finished`]);
//! a move that read it some other way says so, and is not kept. Debug builds
//! check what is remembered against the move made afresh.

use std::collections::BTreeSet;
use std::hash::Hash;
use std::sync::{Arc, RwLock};

use rustc_hash::FxHashMap;

use super::super::super::pack::Packer;
use super::super::super::room::{Heap, block, map_bytes};
use super::super::super::state::{RecordNames, canonical_quietly};
use super::super::super::{Call, Dispatch, JointState};
use super::super::order::values;
use super::super::step::{Certainty, Move, Outcome};
use super::super::{Delivery, JointModel};
use super::mirror::Mirror;
use crate::model::{Fields, ModelString, Renames, TemperModelState};

/// How many steps a thread checks before it starts to skip most of them.
const CHECK_FIRST: u64 = 1 << 12;

/// And then one in this many, so a long walk in a debug build still ends.
const CHECK_EVERY: u64 = 1 << 6;

/// Whether to make this remembered step afresh and compare.
pub(super) fn check_step(steps: u64) -> bool {
    cfg!(feature = "check-every-remembered-step")
        || (cfg!(debug_assertions) && (steps < CHECK_FIRST || steps.is_multiple_of(CHECK_EVERY)))
}

/// A sorted list's change: what left it and what joined it.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Change<T> {
    lost: Vec<Arc<T>>,
    gained: Vec<Arc<T>>,
}

impl<T: Ord> Change<T> {
    /// What changes `before` into `after`; both are sorted.
    fn between(before: &[Arc<T>], after: &[Arc<T>]) -> Self {
        let (mut lost, mut gained) = (Vec::new(), Vec::new());
        let (mut was, mut is) = (before.iter().peekable(), after.iter().peekable());
        loop {
            match (was.peek(), is.peek()) {
                (Some(a), Some(b)) => match a.cmp(b) {
                    std::cmp::Ordering::Equal => {
                        was.next();
                        is.next();
                    }
                    std::cmp::Ordering::Less => lost.extend(was.next().cloned()),
                    std::cmp::Ordering::Greater => gained.extend(is.next().cloned()),
                },
                (Some(_), None) => lost.extend(was.next().cloned()),
                (None, Some(_)) => gained.extend(is.next().cloned()),
                (None, None) => return Self { lost, gained },
            }
        }
    }

    /// `list`, sorted, with this change made, written to `out`.
    fn on(&self, list: &[Arc<T>], out: &mut Vec<Arc<T>>) {
        out.clear();
        let mut lost = self.lost.iter().peekable();
        for held in list {
            if lost.peek().is_some_and(|gone| *gone == held) {
                lost.next();
            } else {
                out.push(held.clone());
            }
        }
        for gained in &self.gained {
            let at = out.partition_point(|held| held < gained);
            out.insert(at, gained.clone());
        }
    }
}

/// Each distinct value once, by number.
struct List<T> {
    items: Vec<T>,
    ids: FxHashMap<T, u32>,
}

impl<T> Default for List<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            ids: FxHashMap::default(),
        }
    }
}

impl<T: Clone + Eq + Hash> List<T> {
    /// The number of `item`, and whether it is new.
    fn id(&mut self, item: T) -> (u32, bool) {
        if let Some(&id) = self.ids.get(&item) {
            return (id, false);
        }
        let id = u32::try_from(self.items.len()).expect("fewer than 2^32 values");
        self.items.push(item.clone());
        self.ids.insert(item, id);
        (id, true)
    }

    /// Bytes its lists take, at their capacity.
    fn bytes(&self) -> usize {
        block(self.items.capacity() * size_of::<T>()) + map_bytes::<T, u32>(self.ids.capacity())
    }
}

/// One way a move goes from a place: the records and timers it leaves, which
/// only the place decides, and what it changes of what is on its way.
#[derive(Clone, Copy)]
struct Way {
    records: u32,
    armed: u32,
    inflight: u32,
    calls: u32,
    certainty: Certainty,
}

/// Where a move's ways are, among the thread's ways. Until the move has been
/// made and kept, none.
#[derive(Clone, Copy)]
pub(super) struct Slot {
    first: u32,
    ways: u32,
}

impl Slot {
    const NONE: Slot = Slot {
        first: 0,
        ways: u32::MAX,
    };

    /// Whether the move has been made from this place and kept.
    pub(super) fn known(self) -> bool {
        self.ways != u32::MAX
    }

    /// The ways of the move, among the thread's ways.
    pub(super) fn ways(self) -> std::ops::Range<usize> {
        if self.known() {
            self.first as usize..self.first as usize + self.ways as usize
        } else {
            0..0
        }
    }
}

/// A set of moves a place has: a number into the lists of moves, and where
/// the ways of each are.
struct Group {
    moves: u32,
    slots: Box<[Slot]>,
}

/// Who holds a set of a state's moves.
#[derive(Clone, Copy)]
pub(super) enum Holder {
    /// The records' moves.
    Records,
    /// The moves of the call with this number.
    Call(u32),
}

/// What one thread of a walk remembers.
#[derive(Default)]
pub(super) struct Memo {
    /// The place each name is, by number.
    places: FxHashMap<Box<[u64]>, u32>,
    place_count: u32,
    /// Where a place's name is written as it is looked up.
    name: Vec<u64>,
    /// The records' moves at each place.
    record_groups: Vec<Option<Group>>,
    /// The moves of each call in flight at each place.
    call_groups: FxHashMap<(u32, u32), Group>,
    /// Where the move of each dispatch from each place goes.
    dispatches: FxHashMap<(u32, u32), Slot>,
    ways: Vec<Way>,
    moves: List<Vec<Move>>,
    records: List<Fields<Arc<TemperModelState>>>,
    armed: List<Fields<u32>>,
    inflight: List<Change<Dispatch>>,
    calls: List<Change<Call>>,
    /// Bytes the values in those lists take, past the lists themselves.
    values: usize,
    /// Bytes of it [`Self::counted`] has said.
    said: usize,
    /// Steps it has made from what it remembers.
    pub(super) steps: u64,
    /// Where a state a move leads to is packed as it is looked up.
    pub(super) packed: Vec<u32>,
    /// Where a state a move leads to is written as it is worked out.
    pub(super) scratch: JointState,
    /// What renumbering comes to for each list of records, once asked.
    names: Vec<Option<Box<RecordNames>>>,
    /// Where names are worked out as a state is renumbered.
    naming: Renames,
    /// The thread's own copy of the values it uses.
    pub(super) mirror: Mirror,
}

impl Memo {
    /// The number of the place `state` is at. What a move reads of a state is
    /// what names it: `held` and `armed`, the numbers of its records and
    /// timers; the values it holds, which the strings a move chooses and makes
    /// up come from; and each family's sequence numbers, which say whether a
    /// count stays exact.
    pub(super) fn place(
        &mut self,
        model: &JointModel,
        (held, armed): (&[u32], &[u32]),
        state: &JointState,
        strings: &BTreeSet<ModelString>,
    ) -> u32 {
        let name = &mut self.name;
        name.clear();
        name.extend(held.iter().chain(armed).map(|&word| u64::from(word)));
        name.push(u64::MAX);
        for value in strings {
            let (kind, word) = match value {
                ModelString::Literal(text) => (0, text.address() as u64),
                ModelString::Symbol(id) => (1, *id as u64),
                ModelString::BlankSymbol(id) => (2, *id as u64),
            };
            name.extend([kind, word]);
        }
        for family in &model.order.families {
            name.push(u64::MAX);
            name.extend(values(family, state).into_iter().map(|n| n as u64));
        }
        if let Some(&place) = self.places.get(name.as_slice()) {
            return place;
        }
        let place = self.place_count;
        self.place_count += 1;
        self.values += block(name.len() * size_of::<u64>());
        self.places.insert(name.as_slice().into(), place);
        place
    }

    /// The group `holder` has at `place`, if its moves are known.
    fn group(&self, place: u32, holder: Holder) -> Option<&Group> {
        match holder {
            Holder::Records => self.record_groups.get(place as usize)?.as_ref(),
            Holder::Call(call) => self.call_groups.get(&(place, call)),
        }
    }

    /// How many moves `holder` has at `place`, if they are known.
    pub(super) fn move_count(&self, place: u32, holder: Holder) -> Option<usize> {
        Some(self.group(place, holder)?.slots.len())
    }

    /// Keep `moves` as those `holder` has at `place`.
    pub(super) fn keep_moves(&mut self, place: u32, holder: Holder, moves: Vec<Move>) {
        let slots: Box<[Slot]> = moves.iter().map(|_| Slot::NONE).collect();
        self.values += block(slots.len() * size_of::<Slot>());
        let bytes = moves.iter().map(Heap::heap).sum::<usize>()
            + block(moves.capacity() * size_of::<Move>());
        let (id, new) = self.moves.id(moves);
        if new {
            self.values += bytes;
        }
        let group = Group { moves: id, slots };
        match holder {
            Holder::Records => {
                if self.record_groups.len() <= place as usize {
                    self.record_groups.resize_with(place as usize + 1, || None);
                }
                self.record_groups[place as usize] = Some(group);
            }
            Holder::Call(call) => {
                self.call_groups.insert((place, call), group);
            }
        }
    }

    /// All the moves `holder` has at `place`.
    pub(super) fn group_moves(&self, place: u32, holder: Holder) -> &[Move] {
        let group = self.group(place, holder).expect("the moves are known");
        &self.moves.items[group.moves as usize]
    }

    /// Where move `at` of those `holder` has at `place` goes.
    pub(super) fn group_slot(&self, place: u32, holder: Holder, at: usize) -> Slot {
        self.group(place, holder)
            .expect("the moves are known")
            .slots[at]
    }

    /// Where the dispatch with this number goes from `place`.
    pub(super) fn dispatch_slot(&self, place: u32, dispatch: u32) -> Slot {
        self.dispatches
            .get(&(place, dispatch))
            .copied()
            .unwrap_or(Slot::NONE)
    }

    /// Where way `at`, of those of a [`Slot`], goes from `state`, written to
    /// the thread's scratch state, and what the model knows of it.
    pub(super) fn way(&mut self, at: usize, state: &JointState) -> (Certainty, u32) {
        let way = self.ways[at];
        let out = &mut self.scratch;
        out.records
            .clone_from(&self.records.items[way.records as usize]);
        out.armed.clone_from(&self.armed.items[way.armed as usize]);
        self.inflight.items[way.inflight as usize].on(&state.inflight, &mut out.inflight);
        self.calls.items[way.calls as usize].on(&state.calls, &mut out.calls);
        (way.certainty, way.records)
    }

    /// What renumbering comes to for list `list` of records.
    fn know_names(&mut self, list: u32, tables: &RwLock<Packer>) {
        let at = list as usize;
        if self.names.len() <= at {
            self.names.resize_with(at + 1, || None);
        }
        if self.names[at].is_none() {
            let mut names = RecordNames::of(&self.records.items[at]);
            for record in names.records.values_mut() {
                *record = self.mirror.own_record(record, tables);
            }
            self.values += names.heap();
            self.names[at] = Some(Box::new(names));
        }
    }

    /// Keep `outcomes` as where a move goes from `state`, and say where.
    fn keep_ways(
        &mut self,
        state: &JointState,
        outcomes: &[Outcome],
        tables: &RwLock<Packer>,
    ) -> Slot {
        let first = u32::try_from(self.ways.len()).expect("fewer than 2^32 ways");
        for (after, _, certainty) in outcomes {
            let mut records = after.records.clone();
            for record in records.values_mut() {
                *record = self.mirror.own_record(record, tables);
            }
            let mut inflight = Change::between(&state.inflight, &after.inflight);
            for dispatch in &mut inflight.gained {
                *dispatch = self.mirror.own_dispatch(dispatch, tables);
            }
            let calls = Change::between(&state.calls, &after.calls);
            let record_bytes = block(records.len() * size_of::<(usize, usize)>())
                + records
                    .values()
                    .zip(state.records.values())
                    .filter(|(is, was)| !Arc::ptr_eq(is, was))
                    .map(|(is, _)| block(size_of::<TemperModelState>()) + is.heap())
                    .sum::<usize>();
            let flight_bytes = (inflight.lost.len() + inflight.gained.len()) * size_of::<usize>()
                + inflight.gained.iter().map(|d| d.heap()).sum::<usize>();
            let call_bytes = (calls.lost.len() + calls.gained.len()) * size_of::<usize>();
            let (records, new) = self.records.id(records);
            self.values += if new { record_bytes } else { 0 };
            let (armed, _) = self.armed.id(after.armed.clone());
            let (inflight, new) = self.inflight.id(inflight);
            self.values += if new { flight_bytes } else { 0 };
            let (calls, new) = self.calls.id(calls);
            self.values += if new { call_bytes } else { 0 };
            self.ways.push(Way {
                records,
                armed,
                inflight,
                calls,
                certainty: *certainty,
            });
        }
        Slot {
            first,
            ways: outcomes.len() as u32,
        }
    }

    /// Keep `outcomes` as where move `at` of `holder`'s at `place` goes.
    pub(super) fn keep_group_move(
        &mut self,
        (place, holder, at): (u32, Holder, usize),
        state: &JointState,
        outcomes: &[Outcome],
        tables: &RwLock<Packer>,
    ) {
        let slot = self.keep_ways(state, outcomes, tables);
        let group = match holder {
            Holder::Records => self.record_groups[place as usize].as_mut(),
            Holder::Call(call) => self.call_groups.get_mut(&(place, call)),
        };
        group.expect("the moves are known").slots[at] = slot;
    }

    /// Keep `outcomes` as where the dispatch with this number goes.
    pub(super) fn keep_dispatch(
        &mut self,
        (place, dispatch): (u32, u32),
        state: &JointState,
        outcomes: &[Outcome],
        tables: &RwLock<Packer>,
    ) {
        let slot = self.keep_ways(state, outcomes, tables);
        self.dispatches.insert((place, dispatch), slot);
    }

    /// Bytes it holds more than it last said. Its lists are counted at their
    /// capacity, as the walk's are.
    pub(super) fn counted(&mut self) -> usize {
        let holds = self.values
            + self.mirror.bytes()
            + block(self.names.capacity() * size_of::<usize>())
            + map_bytes::<Box<[u64]>, u32>(self.places.capacity())
            + block(self.record_groups.capacity() * size_of::<Option<Group>>())
            + map_bytes::<(u32, u32), Group>(self.call_groups.capacity())
            + map_bytes::<(u32, u32), Slot>(self.dispatches.capacity())
            + block(self.ways.capacity() * size_of::<Way>())
            + self.moves.bytes()
            + self.records.bytes()
            + self.armed.bytes()
            + self.inflight.bytes()
            + self.calls.bytes();
        let more = holds.saturating_sub(self.said);
        self.said = holds;
        more
    }
}

impl JointModel {
    /// `state`, which a move left, in its one form, unless it would put more
    /// in flight than the walk keeps: as [`Self::finished`] leaves it.
    pub(super) fn settled_quietly(
        &self,
        memo: &mut Memo,
        list: u32,
        tables: &RwLock<Packer>,
    ) -> bool {
        let state = &mut memo.scratch;
        if self.delivery != Delivery::BeforeNextRequest && !self.fits(state) {
            return false;
        }
        let rewrote = self.order.normalize(state);
        self.in_one_order(state);
        if state.records.same(&memo.records.items[list as usize]) {
            memo.know_names(list, tables);
            let names = memo.names[list as usize]
                .as_ref()
                .expect("its names were just worked out");
            names.renumber(&mut memo.scratch, &mut memo.naming, rewrote);
        } else {
            canonical_quietly(state, rewrote);
        }
        true
    }
}
