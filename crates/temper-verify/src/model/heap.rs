//! Heap estimates shared by local input builders and joint walks.

/// The bytes a heap block of `size` bytes takes: jemalloc's size classes,
/// 16 bytes apart up to 128, then four to each doubling.
pub(crate) fn block(size: usize) -> usize {
    if size <= 128 {
        return size.next_multiple_of(16).max(16);
    }
    let spacing = size.next_power_of_two() / 8;
    size.next_multiple_of(spacing)
}

/// The bytes a `BTreeMap<K, V>` of `len` entries takes in its nodes: eleven
/// entries to a node, and at least five in each node past the first.
pub(crate) fn tree_bytes<K, V>(len: usize) -> usize {
    let nodes = match len {
        0 => 0,
        1..=11 => 1,
        _ => len.div_ceil(5),
    };
    nodes * block(16 + 11 * (size_of::<K>() + size_of::<V>()) + 12 * size_of::<usize>())
}

/// The heap memory a value holds beyond its own size: what a walk counts of
/// the states and moves it builds before it keeps them.
pub(crate) trait Heap {
    fn heap(&self) -> usize;
}

/// The bytes `value` takes, itself and what it holds.
pub(crate) fn whole<T: Heap>(value: &T) -> usize {
    size_of::<T>() + value.heap()
}

macro_rules! no_heap {
    ($($t:ty),*) => {$(impl Heap for $t { fn heap(&self) -> usize { 0 } })*};
}
no_heap!(bool, u32, usize);

impl Heap for String {
    fn heap(&self) -> usize {
        match self.capacity() {
            0 => 0,
            n => block(n),
        }
    }
}

impl<T: Heap> Heap for Option<T> {
    fn heap(&self) -> usize {
        self.as_ref().map_or(0, Heap::heap)
    }
}

impl<T: Heap> Heap for Vec<T> {
    fn heap(&self) -> usize {
        let own = match self.capacity() * size_of::<T>() {
            0 => 0,
            n => block(n),
        };
        own + self.iter().map(Heap::heap).sum::<usize>()
    }
}

impl<K: Heap, V: Heap> Heap for std::collections::BTreeMap<K, V> {
    fn heap(&self) -> usize {
        tree_bytes::<K, V>(self.len())
            + self.iter().map(|(k, v)| k.heap() + v.heap()).sum::<usize>()
    }
}

impl Heap for crate::model::ModelString {
    fn heap(&self) -> usize {
        match self {
            Self::Literal(text) => text.heap(),
            _ => 0,
        }
    }
}

impl Heap for crate::model::ParamValue {
    fn heap(&self) -> usize {
        match self {
            Self::Str(value) => value.heap(),
            _ => 0,
        }
    }
}

impl Heap for crate::model::TemperModelState {
    fn heap(&self) -> usize {
        self.status.heap()
            + self.counters.heap()
            + self.booleans.heap()
            + self.lists.heap()
            + self.strings.heap()
    }
}

impl Heap for crate::model::TemperModelAction {
    fn heap(&self) -> usize {
        self.name.heap() + self.target_state.heap() + self.params.heap()
    }
}

impl<A: Heap, B: Heap> Heap for (A, B) {
    fn heap(&self) -> usize {
        self.0.heap() + self.1.heap()
    }
}

impl<T: Heap> Heap for std::collections::BTreeSet<T> {
    fn heap(&self) -> usize {
        tree_bytes::<T, ()>(self.len()) + self.iter().map(Heap::heap).sum::<usize>()
    }
}

impl<A: Heap, B: Heap, C: Heap> Heap for (A, B, C) {
    fn heap(&self) -> usize {
        self.0.heap() + self.1.heap() + self.2.heap()
    }
}

/// Room for copying a record and applying one transition, including new map
/// nodes, list growth overlap, and the temporary canonical-name map. Charge
/// before cloning; return the unused estimate after the effects are applied.
pub(super) fn step_room(
    state: &super::types::TemperModelState,
    t: &super::types::ResolvedTransition,
    params: &std::collections::BTreeMap<String, super::types::ParamValue>,
) -> usize {
    use super::types::{ModelEffect, ModelString};
    use temper_spec::predicate::{Arg, Literal};
    let additions = t.effects.len() + t.string_params.len();
    let mut room = state.heap()
        + t.to_state.heap()
        + tree_bytes::<String, usize>(state.counters.len() + additions)
        + tree_bytes::<String, bool>(state.booleans.len() + additions)
        + tree_bytes::<String, Vec<ModelString>>(state.lists.len() + additions)
        + tree_bytes::<String, ModelString>(state.strings.len() + additions);
    for effect in &t.effects {
        match effect {
            ModelEffect::SetCounter { var, .. }
            | ModelEffect::AddCounter { var, .. }
            | ModelEffect::SubCounter { var, .. }
            | ModelEffect::SetBool { var, .. }
            | ModelEffect::ListRemoveAt { var, .. } => room += var.heap(),
            ModelEffect::SetString { var, value } => room += var.heap() + value.heap(),
            ModelEffect::ListAppend { var, value } | ModelEffect::ListRemove { var, value } => {
                let element = match value {
                    Arg::Lit(Literal::Str(s)) => s.heap(),
                    Arg::Param(p) => params.get(p).map_or(0, Heap::heap),
                    _ => 0,
                }
                .max(block(t.name.len() + 21));
                let len = state.lists.get(var).map_or(0, Vec::len) + additions;
                room += var.heap() + element + block(2 * len.max(4) * size_of::<ModelString>());
            }
        }
    }
    for param in &t.string_params {
        room += param.name.heap() + params.get(&param.name).map_or(0, Heap::heap);
    }
    let values =
        state.strings.len() + state.lists.values().map(Vec::len).sum::<usize>() + additions;
    room + tree_bytes::<ModelString, ModelString>(values)
}
