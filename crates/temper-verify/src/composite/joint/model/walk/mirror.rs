//! A thread's own copy of the walk's tables.
//!
//! A state's values are counted references, and the walk's threads hand the
//! same few of them round for each of the tens of states a state leads to.
//! Counting them where every thread does would have each thread wait for the
//! others, so each keeps a copy of those it has used, and needs the tables
//! only for a value it has not seen.

use std::hash::Hash;
use std::sync::{Arc, RwLock};

use rustc_hash::FxHashMap;

use super::super::super::pack::{Numbers, Packer, words};
use super::super::super::room::{Heap, block, map_bytes};
use super::super::super::state::{Call, Dispatch, JointState};
use super::shared::{read, write};
use crate::model::{Sym, TemperModelState};

/// A thread's own copies of the values of one table.
struct Side<T> {
    /// Its copy of each value it has used, by the table's number.
    items: Vec<Option<Arc<T>>>,
    /// The numbers of its copies, by where they are. Its copies stay where
    /// they are, since `items` holds them.
    at: FxHashMap<usize, u32>,
    /// The numbers of the values it has seen, by what they hold.
    ids: FxHashMap<Arc<T>, u32>,
    /// Bytes the values take.
    heap: usize,
}

impl<T> Default for Side<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            at: FxHashMap::default(),
            ids: FxHashMap::default(),
            heap: 0,
        }
    }
}

impl<T: Clone + Eq + Hash + Heap> Side<T> {
    fn known(&self, item: &Arc<T>) -> Option<u32> {
        self.at
            .get(&(Arc::as_ptr(item) as *const () as usize))
            .or_else(|| self.ids.get(item))
            .copied()
    }

    /// Its copy of the value numbered `id`, which is `item` in the tables.
    fn own(&mut self, id: u32, item: &Arc<T>) -> Arc<T> {
        let at = id as usize;
        if self.items.len() <= at {
            self.items.resize_with(at + 1, || None);
        }
        if let Some(own) = &self.items[at] {
            return own.clone();
        }
        let own = Arc::new((**item).clone());
        self.items[at] = Some(own.clone());
        self.at.insert(Arc::as_ptr(&own) as *const () as usize, id);
        self.ids.insert(own.clone(), id);
        self.heap += block(2 * size_of::<usize>() + size_of::<T>()) + (*own).heap();
        own
    }

    /// Its copy of the value numbered `id`, when it has one.
    fn get(&self, id: u32) -> Option<&Arc<T>> {
        self.items.get(id as usize)?.as_ref()
    }

    fn bytes(&self) -> usize {
        self.heap
            + block(self.items.capacity() * size_of::<Option<Arc<T>>>())
            + map_bytes::<usize, u32>(self.at.capacity())
            + map_bytes::<Arc<T>, u32>(self.ids.capacity())
    }
}

/// A thread's own copy of the tables, as far as it has used them.
#[derive(Default)]
pub(super) struct Mirror {
    records: Side<TemperModelState>,
    dispatches: Side<Dispatch>,
    calls: Side<Call>,
    types: Vec<Sym>,
    armed: FxHashMap<Sym, u32>,
    armed_types: Vec<Option<Sym>>,
}

impl Numbers for &Mirror {
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

impl Mirror {
    /// `state` packed into `out`, when the thread has seen everything in it.
    pub(super) fn known_into(&self, state: &JointState, out: &mut Vec<u32>) -> bool {
        let mut known = self;
        words(state, &mut known, out).is_some()
    }

    /// Note the numbers of `state`'s values, now that `packed` gives them.
    pub(super) fn learn(&mut self, state: &JointState, packed: &[u32]) {
        let mut input = packed.iter().copied();
        let mut next = || {
            input
                .next()
                .expect("a packed state is read as it was written")
        };
        for record in state.records.values() {
            self.records.own(next(), record);
        }
        next();
        for dispatch in &state.inflight {
            self.dispatches.own(next(), dispatch);
        }
        next();
        for call in &state.calls {
            self.calls.own(next(), call);
        }
        next();
        for (entity, _) in &state.armed {
            let id = next();
            self.armed.insert(*entity, id);
            next();
        }
    }

    /// Its copy of `record`, numbering that in the tables if none has it.
    pub(super) fn own_record(
        &mut self,
        record: &Arc<TemperModelState>,
        tables: &RwLock<Packer>,
    ) -> Arc<TemperModelState> {
        let id = self.records.known(record).unwrap_or_else(|| {
            let known = read(tables).known_record(record);
            known.unwrap_or_else(|| write(tables).number_record(record))
        });
        self.records.own(id, record)
    }

    /// Its copy of `dispatch`, numbering that in the tables if none has it.
    pub(super) fn own_dispatch(
        &mut self,
        dispatch: &Arc<Dispatch>,
        tables: &RwLock<Packer>,
    ) -> Arc<Dispatch> {
        let id = self.dispatches.known(dispatch).unwrap_or_else(|| {
            let known = read(tables).known_dispatch(dispatch);
            known.unwrap_or_else(|| write(tables).number_dispatch(dispatch))
        });
        self.dispatches.own(id, dispatch)
    }

    /// The state `packed` names, from its own copies, reading the tables for
    /// what it has not seen.
    pub(super) fn unpack(&mut self, packed: &[u32], tables: &RwLock<Packer>) -> JointState {
        if self.types.is_empty() {
            self.types.extend_from_slice(read(tables).types());
        }
        let mut input = packed.iter().copied();
        let mut next = || {
            input
                .next()
                .expect("a packed state is read as it was written")
        };
        let records = (0..self.types.len())
            .map(|at| {
                let id = next();
                let own = match self.records.get(id) {
                    Some(own) => own.clone(),
                    None => self.records.own(id, read(tables).record(id)),
                };
                (self.types[at], own)
            })
            .collect();
        let inflight = (0..next())
            .map(|_| {
                let id = next();
                match self.dispatches.get(id) {
                    Some(own) => own.clone(),
                    None => self.dispatches.own(id, read(tables).dispatch(id)),
                }
            })
            .collect();
        let calls = (0..next())
            .map(|_| {
                let id = next();
                match self.calls.get(id) {
                    Some(own) => own.clone(),
                    None => self.calls.own(id, read(tables).call(id)),
                }
            })
            .collect();
        let armed = (0..next())
            .map(|_| {
                let id = next();
                let by = next();
                let at = id as usize;
                if self.armed_types.len() <= at {
                    self.armed_types.resize(at + 1, None);
                }
                let entity = *self.armed_types[at].get_or_insert_with(|| {
                    let entity = read(tables).armed_type(id);
                    self.armed.insert(entity, id);
                    entity
                });
                (entity, by)
            })
            .collect();
        JointState {
            records,
            inflight,
            calls,
            armed,
        }
    }

    /// Bytes it holds.
    pub(super) fn bytes(&self) -> usize {
        self.records.bytes()
            + self.dispatches.bytes()
            + self.calls.bytes()
            + map_bytes::<Sym, u32>(self.armed.capacity())
    }
}
