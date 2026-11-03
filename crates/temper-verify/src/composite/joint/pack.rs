//! Joint states packed small for the walk. Every string is a number in one
//! table, so a walked state is a short run of numbers rather than maps on
//! the heap: a state of the Katagami control specs takes about 12.7 KB
//! unpacked and a few hundred bytes packed.

use std::collections::{BTreeMap, HashMap};
use std::rc::Rc;
use std::slice::Iter;

use super::state::{Call, Dispatch, JointState, ReactionStep};
use crate::model::{ModelString, ParamValue, TemperModelState};

/// A packed joint state, shared by the walk's list and its index.
pub(super) type Packed = Rc<[u32]>;

/// The table of strings packed states refer to.
#[derive(Default)]
pub(super) struct Packer {
    ids: HashMap<String, u32>,
    names: Vec<String>,
    /// Bytes the strings take on the heap, each held twice.
    pub(super) heap: usize,
}

impl Packer {
    pub(super) fn pack(&mut self, state: &JointState) -> Packed {
        let mut out = Vec::new();
        state.put(self, &mut out);
        out.into()
    }

    pub(super) fn unpack(&self, packed: &[u32]) -> JointState {
        JointState::take(self, &mut packed.iter())
    }

    /// The number for `name`, added to the table when new.
    pub(super) fn id(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.ids.get(name) {
            return id;
        }
        let id = u32::try_from(self.names.len()).expect("fewer than 2^32 strings");
        self.ids.insert(name.to_string(), id);
        self.names.push(name.to_string());
        self.heap += 2 * super::room::block(name.len());
        id
    }

    pub(super) fn name(&self, id: u32) -> &str {
        &self.names[id as usize]
    }

    /// Bytes the table holds: its strings, and its list and index at their
    /// capacity.
    pub(super) fn bytes(&self) -> usize {
        self.heap
            + self.names.capacity() * size_of::<String>()
            + super::room::map_bytes::<String, u32>(self.ids.capacity())
    }
}

/// A value written as numbers and read back.
trait Pack: Sized {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u32>);
    fn take(packer: &Packer, input: &mut Iter<'_, u32>) -> Self;
}

fn next(input: &mut Iter<'_, u32>) -> u32 {
    *input
        .next()
        .expect("a packed state is read as it was written")
}

impl Pack for u32 {
    fn put(&self, _: &mut Packer, out: &mut Vec<u32>) {
        out.push(*self);
    }
    fn take(_: &Packer, input: &mut Iter<'_, u32>) -> Self {
        next(input)
    }
}

impl Pack for usize {
    fn put(&self, _: &mut Packer, out: &mut Vec<u32>) {
        out.push(u32::try_from(*self).expect("the model bounds its counts"));
    }
    fn take(_: &Packer, input: &mut Iter<'_, u32>) -> Self {
        next(input) as usize
    }
}

impl Pack for bool {
    fn put(&self, _: &mut Packer, out: &mut Vec<u32>) {
        out.push(u32::from(*self));
    }
    fn take(_: &Packer, input: &mut Iter<'_, u32>) -> Self {
        next(input) != 0
    }
}

impl Pack for String {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u32>) {
        out.push(packer.id(self));
    }
    fn take(packer: &Packer, input: &mut Iter<'_, u32>) -> Self {
        packer.name(next(input)).to_string()
    }
}

impl Pack for ModelString {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u32>) {
        match self {
            ModelString::Literal(s) => {
                out.push(0);
                s.put(packer, out);
            }
            ModelString::Symbol(n) => {
                out.push(1);
                n.put(packer, out);
            }
            ModelString::BlankSymbol(n) => {
                out.push(2);
                n.put(packer, out);
            }
        }
    }
    fn take(packer: &Packer, input: &mut Iter<'_, u32>) -> Self {
        match next(input) {
            0 => Self::Literal(String::take(packer, input)),
            1 => Self::Symbol(usize::take(packer, input)),
            2 => Self::BlankSymbol(usize::take(packer, input)),
            _ => unreachable!("unknown packed string kind"),
        }
    }
}

impl<T: Pack> Pack for Option<T> {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u32>) {
        out.push(u32::from(self.is_some()));
        if let Some(value) = self {
            value.put(packer, out);
        }
    }
    fn take(packer: &Packer, input: &mut Iter<'_, u32>) -> Self {
        (next(input) != 0).then(|| T::take(packer, input))
    }
}

impl<T: Pack> Pack for Vec<T> {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u32>) {
        self.len().put(packer, out);
        for value in self {
            value.put(packer, out);
        }
    }
    fn take(packer: &Packer, input: &mut Iter<'_, u32>) -> Self {
        let len = usize::take(packer, input);
        (0..len).map(|_| T::take(packer, input)).collect()
    }
}

impl<K: Pack + Ord, V: Pack> Pack for BTreeMap<K, V> {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u32>) {
        self.len().put(packer, out);
        for (key, value) in self {
            key.put(packer, out);
            value.put(packer, out);
        }
    }
    fn take(packer: &Packer, input: &mut Iter<'_, u32>) -> Self {
        let len = usize::take(packer, input);
        (0..len)
            .map(|_| (K::take(packer, input), V::take(packer, input)))
            .collect()
    }
}

impl Pack for ParamValue {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u32>) {
        match self {
            ParamValue::Count(count) => {
                out.push(0);
                count.put(packer, out);
            }
            ParamValue::Bool(value) => {
                out.push(1);
                value.put(packer, out);
            }
            ParamValue::Str(text) => {
                out.push(2);
                text.put(packer, out);
            }
            ParamValue::Fresh => out.push(3),
        }
    }
    fn take(packer: &Packer, input: &mut Iter<'_, u32>) -> Self {
        match next(input) {
            0 => ParamValue::Count(usize::take(packer, input)),
            1 => ParamValue::Bool(bool::take(packer, input)),
            2 => ParamValue::Str(ModelString::take(packer, input)),
            _ => ParamValue::Fresh,
        }
    }
}

impl Pack for TemperModelState {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u32>) {
        self.status.put(packer, out);
        self.counters.put(packer, out);
        self.booleans.put(packer, out);
        self.lists.put(packer, out);
        self.strings.put(packer, out);
    }
    fn take(packer: &Packer, input: &mut Iter<'_, u32>) -> Self {
        TemperModelState {
            status: Pack::take(packer, input),
            counters: Pack::take(packer, input),
            booleans: Pack::take(packer, input),
            lists: Pack::take(packer, input),
            strings: Pack::take(packer, input),
        }
    }
}

impl Pack for ReactionStep {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u32>) {
        self.entity_type.put(packer, out);
        self.action.put(packer, out);
        self.generated_for.put(packer, out);
    }
    fn take(packer: &Packer, input: &mut Iter<'_, u32>) -> Self {
        Self {
            entity_type: Pack::take(packer, input),
            action: Pack::take(packer, input),
            generated_for: Pack::take(packer, input),
        }
    }
}

impl Pack for super::state::Committed {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u32>) {
        self.status.put(packer, out);
        self.fields.put(packer, out);
    }
    fn take(packer: &Packer, input: &mut Iter<'_, u32>) -> Self {
        Self {
            status: Pack::take(packer, input),
            fields: Pack::take(packer, input),
        }
    }
}

impl Pack for Dispatch {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u32>) {
        self.to.put(packer, out);
        self.action.put(packer, out);
        self.target_state.put(packer, out);
        self.params.put(packer, out);
        self.read.put(packer, out);
        self.chain.put(packer, out);
        self.via.put(packer, out);
        self.by.put(packer, out);
        self.unread_guard.put(packer, out);
    }
    fn take(packer: &Packer, input: &mut Iter<'_, u32>) -> Self {
        Dispatch {
            to: Pack::take(packer, input),
            action: Pack::take(packer, input),
            target_state: Pack::take(packer, input),
            params: Pack::take(packer, input),
            read: Pack::take(packer, input),
            chain: Pack::take(packer, input),
            via: Pack::take(packer, input),
            by: Pack::take(packer, input),
            unread_guard: Pack::take(packer, input),
        }
    }
}

impl Pack for Call {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u32>) {
        self.to.put(packer, out);
        self.module.put(packer, out);
        self.on_success.put(packer, out);
        self.on_failure.put(packer, out);
        self.by.put(packer, out);
        self.asked.put(packer, out);
    }
    fn take(packer: &Packer, input: &mut Iter<'_, u32>) -> Self {
        Call {
            to: Pack::take(packer, input),
            module: Pack::take(packer, input),
            on_success: Pack::take(packer, input),
            on_failure: Pack::take(packer, input),
            by: Pack::take(packer, input),
            asked: Pack::take(packer, input),
        }
    }
}

impl Pack for JointState {
    fn put(&self, packer: &mut Packer, out: &mut Vec<u32>) {
        self.records.put(packer, out);
        self.inflight.put(packer, out);
        self.calls.put(packer, out);
        self.armed.put(packer, out);
    }
    fn take(packer: &Packer, input: &mut Iter<'_, u32>) -> Self {
        JointState {
            records: Pack::take(packer, input),
            inflight: Pack::take(packer, input),
            calls: Pack::take(packer, input),
            armed: Pack::take(packer, input),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packed_call_snapshots_keep_literals_distinct_from_symbols() {
        let state = JointState {
            records: BTreeMap::new(),
            inflight: Vec::new(),
            calls: vec![Call {
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
            }],
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
