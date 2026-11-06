//! Joint states packed small for the walk. A state of a walk differs from the
//! next in a record or two, so each distinct record, dispatch and call is kept
//! once in a table and a walked state is a short run of numbers into them.

use rustc_hash::FxHashMap;
use std::collections::HashMap;

use std::hash::Hash;
use std::sync::Arc;

use super::room::{Heap, block, map_bytes};
use super::state::{Call, Dispatch, JointState};
use crate::model::{Sym, TemperModelState};

/// A packed joint state, shared by the walk's list and its index.
pub(super) type Packed = Arc<[u32]>;

/// Each distinct value once.
struct Table<T> {
    items: Vec<Arc<T>>,
    ids: FxHashMap<Arc<T>, u32>,
    /// The values `items` holds, by where they are. Those `items` keeps stay
    /// where they are, and a state unpacked holds these same ones, so most
    /// values packed are found here without being read.
    at: FxHashMap<usize, u32>,
    /// Bytes the values take on the heap, each with its count of holders.
    heap: usize,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            ids: FxHashMap::default(),
            at: FxHashMap::default(),
            heap: 0,
        }
    }
}

impl<T: Eq + Hash + Heap> Table<T> {
    /// Where `item` is. `items` keeps what `at` names, so no other value
    /// takes its place.
    fn address(item: &Arc<T>) -> usize {
        Arc::as_ptr(item) as *const () as usize
    }

    /// The id of `item`, if the table holds it.
    fn known(&self, item: &Arc<T>) -> Option<u32> {
        self.at
            .get(&Self::address(item))
            .or_else(|| self.ids.get(item))
            .copied()
    }

    fn id(&mut self, item: &Arc<T>) -> u32 {
        if let Some(id) = self.known(item) {
            return id;
        }
        let id = u32::try_from(self.items.len()).expect("fewer than 2^32 values");
        self.items.push(item.clone());
        self.ids.insert(item.clone(), id);
        self.at.insert(Self::address(item), id);
        self.heap += block(2 * size_of::<usize>() + size_of::<T>()) + (**item).heap();
        id
    }

    fn bytes(&self) -> usize {
        self.heap
            + block(self.items.capacity() * size_of::<Arc<T>>())
            + map_bytes::<Arc<T>, u32>(self.ids.capacity())
            + map_bytes::<usize, u32>(self.at.capacity())
    }
}

/// What numbers a state's values when it is packed.
pub(super) trait Numbers {
    fn record(&mut self, record: &Arc<TemperModelState>) -> Option<u32>;
    fn dispatch(&mut self, dispatch: &Arc<Dispatch>) -> Option<u32>;
    fn call(&mut self, call: &Arc<Call>) -> Option<u32>;
    fn armed(&mut self, entity: Sym) -> Option<u32>;
}

/// Numbers known already, and none for a value that is new.
impl Numbers for &Packer {
    fn record(&mut self, record: &Arc<TemperModelState>) -> Option<u32> {
        self.records.known(record)
    }
    fn dispatch(&mut self, dispatch: &Arc<Dispatch>) -> Option<u32> {
        self.dispatches.known(dispatch)
    }
    fn call(&mut self, call: &Arc<Call>) -> Option<u32> {
        self.calls.known(call)
    }
    fn armed(&mut self, entity: Sym) -> Option<u32> {
        self.armed.get(&entity).copied()
    }
}

/// Numbers known already, and a new one for a value that is new.
impl Numbers for Packer {
    fn record(&mut self, record: &Arc<TemperModelState>) -> Option<u32> {
        Some(self.records.id(record))
    }
    fn dispatch(&mut self, dispatch: &Arc<Dispatch>) -> Option<u32> {
        Some(self.dispatches.id(dispatch))
    }
    fn call(&mut self, call: &Arc<Call>) -> Option<u32> {
        Some(self.calls.id(call))
    }
    fn armed(&mut self, entity: Sym) -> Option<u32> {
        let next = u32::try_from(self.armed_types.len()).expect("fewer than 2^32 types");
        let id = *self.armed.entry(entity).or_insert(next);
        if id == next {
            self.armed_types.push(entity);
        }
        Some(id)
    }
}

/// `state` as a short run of numbers, written to `out`, or false when
/// `numbers` has none for a value in it.
pub(super) fn words(
    state: &JointState,
    numbers: &mut impl Numbers,
    out: &mut Vec<u32>,
) -> Option<()> {
    out.clear();
    for record in state.records.values() {
        out.push(numbers.record(record)?);
    }
    out.push(u32::try_from(state.inflight.len()).expect("the model bounds what is in flight"));
    for dispatch in &state.inflight {
        out.push(numbers.dispatch(dispatch)?);
    }
    out.push(u32::try_from(state.calls.len()).expect("the model bounds what is in flight"));
    for call in &state.calls {
        out.push(numbers.call(call)?);
    }
    out.push(u32::try_from(state.armed.len()).expect("one timer to a record"));
    for (entity, by) in &state.armed {
        out.push(numbers.armed(*entity)?);
        out.push(*by);
    }
    Some(())
}

/// A packed state's parts: its records, what is in flight, its calls and its
/// timers armed.
pub(super) fn parts(packed: &[u32], records: usize) -> (&[u32], &[u32], &[u32], &[u32]) {
    let (held, rest) = packed.split_at(records);
    let (flying, rest) = rest[1..].split_at(rest[0] as usize);
    let (calls, rest) = rest[1..].split_at(rest[0] as usize);
    (held, flying, calls, &rest[1..])
}

/// The tables packed states refer to.
#[derive(Default)]
pub(super) struct Packer {
    ids: HashMap<String, u32>,
    names: Vec<String>,
    /// Bytes the strings take on the heap, each held twice.
    heap: usize,
    /// The types of the records every state of the walk holds, in order. A
    /// move replaces a record and never adds one.
    types: Vec<Sym>,
    /// The types armed timers name.
    armed: HashMap<Sym, u32>,
    armed_types: Vec<Sym>,
    records: Table<TemperModelState>,
    dispatches: Table<Dispatch>,
    calls: Table<Call>,
}

impl Packer {
    /// `state` packed, its new values added to the tables.
    pub(super) fn pack(&mut self, state: &JointState) -> Packed {
        let mut out = Vec::new();
        self.pack_into(state, &mut out);
        out.into()
    }

    /// `state` packed into `out`, its new values added to the tables.
    pub(super) fn pack_into(&mut self, state: &JointState, out: &mut Vec<u32>) {
        if self.types.is_empty() {
            self.types.extend(state.records.keys().copied());
        }
        words(state, self, out).expect("a packer adds what it does not know");
    }

    /// `state` packed into `out`, when the tables hold everything in it
    /// already. A walk's threads read the tables together and take them alone
    /// only for the few values none has seen.
    pub(super) fn known_into(&self, state: &JointState, out: &mut Vec<u32>) -> bool {
        let mut known = self;
        words(state, &mut known, out).is_some()
    }

    /// The number of `record`, if the tables hold it.
    pub(super) fn known_record(&self, record: &Arc<TemperModelState>) -> Option<u32> {
        self.records.known(record)
    }

    /// The number of `record`, added to the tables if they do not hold it.
    pub(super) fn number_record(&mut self, record: &Arc<TemperModelState>) -> u32 {
        self.records.id(record)
    }

    /// The number of `dispatch`, if the tables hold it.
    pub(super) fn known_dispatch(&self, dispatch: &Arc<Dispatch>) -> Option<u32> {
        self.dispatches.known(dispatch)
    }

    /// The number of `dispatch`, added to the tables if they do not hold it.
    pub(super) fn number_dispatch(&mut self, dispatch: &Arc<Dispatch>) -> u32 {
        self.dispatches.id(dispatch)
    }

    /// The record numbered `id`.
    pub(super) fn record(&self, id: u32) -> &Arc<TemperModelState> {
        &self.records.items[id as usize]
    }

    /// The dispatch numbered `id`.
    pub(super) fn dispatch(&self, id: u32) -> &Arc<Dispatch> {
        &self.dispatches.items[id as usize]
    }

    /// The call numbered `id`.
    pub(super) fn call(&self, id: u32) -> &Arc<Call> {
        &self.calls.items[id as usize]
    }

    /// The type of the timers numbered `id`.
    pub(super) fn armed_type(&self, id: u32) -> Sym {
        self.armed_types[id as usize]
    }

    /// The types of the records every state of the walk holds, in order.
    pub(super) fn types(&self) -> &[Sym] {
        &self.types
    }

    pub(super) fn unpack(&self, packed: &[u32]) -> JointState {
        let mut input = packed.iter().map(|&word| word as usize);
        let mut next = || {
            input
                .next()
                .expect("a packed state is read as it was written")
        };
        let records = self
            .types
            .iter()
            .map(|entity| (*entity, self.records.items[next()].clone()))
            .collect();
        let inflight = (0..next())
            .map(|_| self.dispatches.items[next()].clone())
            .collect();
        let calls = (0..next())
            .map(|_| self.calls.items[next()].clone())
            .collect();
        let armed = (0..next())
            .map(|_| (self.armed_types[next()], next() as u32))
            .collect();
        JointState {
            records,
            inflight,
            calls,
            armed,
        }
    }

    /// The number for `name`, added to the table when new.
    pub(super) fn id(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = u32::try_from(self.names.len()).expect("fewer than 2^32 strings");
        self.ids.insert(name.to_string(), id);
        self.names.push(name.to_string());
        self.heap += 2 * block(name.len());
        id
    }

    pub(super) fn name(&self, id: u32) -> &str {
        &self.names[id as usize]
    }

    /// Bytes the tables hold: their values, and their lists and indexes at
    /// their capacity.
    pub(super) fn bytes(&self) -> usize {
        self.heap
            + self.names.capacity() * size_of::<String>()
            + map_bytes::<String, u32>(self.ids.capacity())
            + self.records.bytes()
            + self.dispatches.bytes()
            + self.calls.bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Fields, ModelString};

    #[test]
    fn packed_call_snapshots_keep_literals_distinct_from_symbols() {
        let state = JointState {
            records: Fields::new(),
            inflight: Vec::new(),
            calls: vec![Arc::new(Call {
                to: "T".into(),
                module: "worker".into(),
                on_success: std::sync::Arc::new([]),
                on_failure: None,
                by: 0,
                caller: None,
                asked: Fields::from([
                    ("literal", ModelString::from("#1")),
                    ("fresh", ModelString::Symbol(1)),
                    ("blank", ModelString::BlankSymbol(2)),
                ]),
            })],
            armed: Fields::new(),
        };
        let mut packer = Packer::default();
        let packed = packer.pack(&state);
        let restored = packer.unpack(&packed);
        assert_eq!(restored, state);
        assert_ne!(
            restored.calls[0].asked["literal"],
            restored.calls[0].asked["fresh"]
        );
    }
}
