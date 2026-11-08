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

/// What a value shared with others holds is theirs too: it counts here only
/// while nothing else holds it.
impl<T: Heap> Heap for std::sync::Arc<T> {
    fn heap(&self) -> usize {
        match std::sync::Arc::strong_count(self) {
            1 => block(2 * size_of::<usize>() + size_of::<T>()) + (**self).heap(),
            _ => 0,
        }
    }
}

impl Heap for std::sync::Arc<str> {
    fn heap(&self) -> usize {
        match std::sync::Arc::strong_count(self) {
            1 => block(2 * size_of::<usize>() + self.len()),
            _ => 0,
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

impl Heap for super::fields::Sym {
    fn heap(&self) -> usize {
        0
    }
}

/// A list shared with others counts as [`std::sync::Arc`] does.
impl<V: Heap> Heap for super::fields::Fields<V> {
    fn heap(&self) -> usize {
        self.held(|entries| {
            block(4 * size_of::<usize>())
                + block(entries.capacity() * size_of::<(super::fields::Sym, V)>())
                + entries.iter().map(|(_, value)| value.heap()).sum::<usize>()
        })
    }
}

impl<T> Heap for std::sync::Arc<[T]> {
    fn heap(&self) -> usize {
        match std::sync::Arc::strong_count(self) {
            1 => block(2 * size_of::<usize>() + self.len() * size_of::<T>()),
            _ => 0,
        }
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
        0
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

/// Room for copying a record and applying one transition: what it writes is
/// copied whole, one list for each kind of field. Charge before cloning;
/// return the unused estimate after the effects are applied.
pub(super) fn step_room(
    state: &super::types::TemperModelState,
    t: &super::types::ResolvedTransition,
) -> usize {
    use super::fields::Sym;
    use super::types::ModelString;
    let additions = t.effects.len() + t.string_params.len();
    // A push doubles a full list, and starts an empty one at four.
    let list = |len: usize| block(len.max(4) * size_of::<ModelString>());
    // As [`Fields`](super::fields::Fields) holds them: one shared list, its
    // entries padded as pairs. Copied, it holds just its entries, and a new
    // name doubles it, starting at four.
    let entries = |len: usize, pair: usize| {
        block(4 * size_of::<usize>()) + block((2 * (len + additions)).max(4) * pair)
    };
    entries(state.counters.len(), size_of::<(Sym, usize)>())
        + entries(state.booleans.len(), size_of::<(Sym, bool)>())
        + entries(state.lists.len(), size_of::<(Sym, Vec<ModelString>)>())
        + entries(state.strings.len(), size_of::<(Sym, ModelString)>())
        + state
            .lists
            .values()
            .map(|items| list((items.len() + additions) * 2))
            .sum::<usize>()
        // An effect can start a list the record doesn't hold yet.
        + additions * list(additions * 2)
}
