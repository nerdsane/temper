//! Joint states packed small for the walk. A state of a walk differs from the
//! next in a record or two, so each distinct record, dispatch and call is kept
//! once in a table and a walked state is a short run of numbers into them.

use std::collections::{BTreeMap, HashMap};
use std::hash::Hash;
use std::sync::Arc;

use super::room::{Heap, block, map_bytes};
use super::state::{Call, Dispatch, JointState};
use crate::model::TemperModelState;

/// A packed joint state, shared by the walk's list and its index.
pub(super) type Packed = Arc<[u32]>;

/// Each distinct value once.
struct Table<T> {
    items: Vec<Arc<T>>,
    ids: HashMap<Arc<T>, u32>,
    /// The values `items` holds, by where they are. Those `items` keeps stay
    /// where they are, and a state unpacked holds these same ones, so most
    /// values packed are found here without being read.
    at: HashMap<usize, u32>,
    /// Bytes the values take on the heap, each with its count of holders.
    heap: usize,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            ids: HashMap::new(),
            at: HashMap::new(),
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
trait Numbers {
    fn record(&mut self, record: &Arc<TemperModelState>) -> Option<u32>;
    fn dispatch(&mut self, dispatch: &Arc<Dispatch>) -> Option<u32>;
    fn call(&mut self, call: &Arc<Call>) -> Option<u32>;
    fn armed(&mut self, entity: &Arc<str>) -> Option<u32>;
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
    fn armed(&mut self, entity: &Arc<str>) -> Option<u32> {
        self.armed.get(entity).copied()
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
    fn armed(&mut self, entity: &Arc<str>) -> Option<u32> {
        let next = u32::try_from(self.armed_types.len()).expect("fewer than 2^32 types");
        let id = *self.armed.entry(entity.clone()).or_insert(next);
        if id == next {
            self.armed_types.push(entity.clone());
        }
        Some(id)
    }
}

/// `state` as a short run of numbers, or none when `numbers` has none for a
/// value in it.
fn words(state: &JointState, numbers: &mut impl Numbers) -> Option<Packed> {
    let mut out = Vec::with_capacity(
        state.records.len() + 3 + state.inflight.len() + state.calls.len() + 2 * state.armed.len(),
    );
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
        out.push(numbers.armed(entity)?);
        out.push(*by);
    }
    Some(out.into())
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
    types: Vec<Arc<str>>,
    /// The types armed timers name.
    armed: HashMap<Arc<str>, u32>,
    armed_types: Vec<Arc<str>>,
    records: Table<TemperModelState>,
    dispatches: Table<Dispatch>,
    calls: Table<Call>,
}

impl Packer {
    /// `state` packed, its new values added to the tables.
    pub(super) fn pack(&mut self, state: &JointState) -> Packed {
        if self.types.is_empty() {
            self.types.extend(state.records.keys().cloned());
        }
        words(state, self).expect("a packer adds what it does not know")
    }

    /// `state` packed, when the tables hold everything in it already. A
    /// walk's threads read the tables together and take them alone only for
    /// the few values none has seen.
    pub(super) fn known(&self, state: &JointState) -> Option<Packed> {
        let mut known = self;
        words(state, &mut known)
    }

    /// The number of `name`, when the tables hold it.
    pub(super) fn known_id(&self, name: &str) -> Option<u32> {
        self.ids.get(name).copied()
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
            .map(|entity| (entity.clone(), self.records.items[next()].clone()))
            .collect();
        let inflight = (0..next())
            .map(|_| self.dispatches.items[next()].clone())
            .collect();
        let calls = (0..next())
            .map(|_| self.calls.items[next()].clone())
            .collect();
        let armed = (0..next())
            .map(|_| (self.armed_types[next()].clone(), next() as u32))
            .collect::<BTreeMap<_, _>>();
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
    use crate::model::ModelString;

    #[test]
    fn packed_call_snapshots_keep_literals_distinct_from_symbols() {
        let state = JointState {
            records: BTreeMap::new(),
            inflight: Vec::new(),
            calls: vec![Arc::new(Call {
                to: "T".into(),
                module: "worker".into(),
                on_success: Vec::new(),
                on_failure: None,
                by: 0,
                asked: BTreeMap::from([
                    ("literal".into(), ModelString::from("#1")),
                    ("fresh".into(), ModelString::Symbol(1)),
                    ("blank".into(), ModelString::BlankSymbol(2)),
                ]),
            })],
            armed: BTreeMap::new(),
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
