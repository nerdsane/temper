//! What each state of the model check's whole walk can still reach.
//!
//! The walk keeps its graph. Its strongly connected components give each
//! state the places it can still reach, and whether a step past a counter's
//! bound lies somewhere ahead of it: what lies past a bound was not walked,
//! so what such a state can reach is not known.

use crate::model::TemperModel;
use crate::model::TemperModelState;

/// The states a whole walk found, each with the place it is in, whether a
/// step from it was left out past a bound, how it was first reached, and the
/// states it steps to. A state's number is the order it was found in, and a
/// state's steps are added when the walk takes it, in that order.
#[derive(Debug, Default)]
pub(crate) struct Graph {
    pub(crate) place: Vec<u32>,
    pub(crate) cut: Vec<bool>,
    pub(crate) parent: Vec<Option<(u32, u32)>>,
    pub(crate) depth: Vec<u32>,
    offsets: Vec<u32>,
    steps: Vec<u32>,
}

/// A state's place when its status is not a declared place.
pub(crate) const NO_PLACE: u32 = u32::MAX;

impl Graph {
    /// Number a newly found state, first reached from `parent` by the
    /// transition at that index, or a start state.
    pub(crate) fn add(
        &mut self,
        model: &TemperModel,
        state: &TemperModelState,
        parent: Option<(u32, u32)>,
    ) -> u32 {
        let at = u32::try_from(self.place.len()).expect("a walk numbers fewer than 2^32 states");
        let place = model.states.iter().position(|s| *s == state.status);
        self.place
            .push(place.map_or(NO_PLACE, |p| u32::try_from(p).expect("places fit in u32")));
        self.cut.push(false);
        self.depth
            .push(parent.map_or(0, |(from, _)| self.depth[from as usize] + 1));
        self.parent.push(parent);
        at
    }

    /// The steps from state `at`, the next state the walk takes, and whether
    /// one was left out past a bound.
    pub(crate) fn steps_from(&mut self, at: u32, cut: bool, mut to: Vec<u32>) {
        if self.offsets.is_empty() {
            self.offsets.push(0);
        }
        assert_eq!(
            at as usize,
            self.offsets.len() - 1,
            "the walk takes states in the order it found them"
        );
        to.sort_unstable();
        to.dedup();
        self.cut[at as usize] = cut;
        self.steps.extend(to);
        self.offsets
            .push(u32::try_from(self.steps.len()).expect("a walk has fewer than 2^32 steps"));
    }

    pub(crate) fn len(&self) -> usize {
        self.place.len()
    }

    pub(crate) fn steps(&self, at: usize) -> &[u32] {
        &self.steps[self.offsets[at] as usize..self.offsets[at + 1] as usize]
    }

    /// The states and transitions from a start state to `at`: each state's
    /// number with the index of the transition taken from it, the last with
    /// none.
    pub(crate) fn path_to(&self, mut at: usize) -> Vec<(usize, Option<usize>)> {
        let mut path = vec![(at, None)];
        while let Some((from, t)) = self.parent[at] {
            path.push((from as usize, Some(t as usize)));
            at = from as usize;
        }
        path.reverse();
        path
    }

    /// The moves from a start state to `at`.
    pub(crate) fn path(&self, model: &TemperModel, at: usize) -> Vec<String> {
        self.path_to(at)
            .into_iter()
            .filter_map(|(_, t)| Some(model.transitions[t?].name.to_string()))
            .collect()
    }
}

/// Places as a set of bits, one word per 64 places.
#[derive(Clone, Copy)]
pub(crate) struct Bits {
    pub(crate) words: usize,
}

impl Bits {
    pub(crate) fn for_model(model: &TemperModel) -> Self {
        Bits {
            words: model.states.len().div_ceil(64).max(1),
        }
    }

    pub(crate) fn row<'a>(&self, all: &'a [u64], i: usize) -> &'a [u64] {
        &all[i * self.words..(i + 1) * self.words]
    }

    /// The set holding the places named in `names`.
    pub(crate) fn of(&self, model: &TemperModel, names: &[String]) -> Vec<u64> {
        let mut set = vec![0u64; self.words];
        for p in (0..model.states.len()).filter(|&p| names.contains(&model.states[p])) {
            set[p / 64] |= 1 << (p % 64);
        }
        set
    }

    pub(crate) fn names(&self, model: &TemperModel, set: &[u64]) -> Vec<String> {
        (0..model.states.len())
            .filter(|&p| set[p / 64] & (1 << (p % 64)) != 0)
            .map(|p| model.states[p].clone())
            .collect()
    }
}

/// The places each state of a whole walk can still reach, its own among
/// them, and whether a step past a bound lies ahead of it.
pub(crate) struct Reach {
    pub(crate) bits: Bits,
    pub(crate) component: Vec<u32>,
    pub(crate) places: Vec<u64>,
    past_bound: Vec<bool>,
    pub(crate) first: Vec<usize>,
    pub(crate) members: Vec<usize>,
}

impl Reach {
    /// What each state in `graph` can reach, or `None` when the walk did not
    /// add every state's steps.
    pub(crate) fn of(model: &TemperModel, graph: &Graph) -> Option<Self> {
        let n = graph.len();
        if n == 0 || graph.offsets.len() != n + 1 {
            return None;
        }
        let bits = Bits::for_model(model);
        let (component, count) = components(graph);

        // A component is numbered after every one it steps to, so each one's
        // row is built from rows already done.
        let mut first = vec![0usize; count + 1];
        for &c in &component {
            first[c as usize + 1] += 1;
        }
        for c in 0..count {
            first[c + 1] += first[c];
        }
        let mut members = vec![0usize; n];
        let mut next = first.clone();
        for (at, &c) in component.iter().enumerate() {
            members[next[c as usize]] = at;
            next[c as usize] += 1;
        }
        let mut places = vec![0u64; count * bits.words];
        let mut past_bound = vec![false; count];
        for c in 0..count {
            let mut row = vec![0u64; bits.words];
            let mut cut = false;
            for &at in &members[first[c]..first[c + 1]] {
                let place = graph.place[at];
                if place != NO_PLACE {
                    row[place as usize / 64] |= 1 << (place % 64);
                }
                cut |= graph.cut[at];
                for &to in graph.steps(at) {
                    let d = component[to as usize] as usize;
                    if d != c {
                        for (w, word) in row.iter_mut().enumerate() {
                            *word |= bits.row(&places, d)[w];
                        }
                        cut |= past_bound[d];
                    }
                }
            }
            places[c * bits.words..(c + 1) * bits.words].copy_from_slice(&row);
            past_bound[c] = cut;
        }
        Some(Reach {
            bits,
            component,
            places,
            past_bound,
            first,
            members,
        })
    }

    /// The places state `at` can reach.
    pub(crate) fn places(&self, at: usize) -> &[u64] {
        self.bits.row(&self.places, self.component[at] as usize)
    }

    /// Whether a step past a bound lies ahead of state `at`.
    pub(crate) fn past_bound(&self, at: usize) -> bool {
        self.past_bound[self.component[at] as usize]
    }
}

/// The strongly connected components of `graph` (Tarjan's, without
/// recursion): each state's component, numbered after every component it
/// steps to, and how many there are.
fn components(graph: &Graph) -> (Vec<u32>, usize) {
    let n = graph.len();
    let mut t = Tarjan {
        order: vec![UNSEEN; n],
        low: vec![0; n],
        open: Vec::new(),
        calls: Vec::new(),
        seen: 0,
    };
    let mut component = vec![UNSEEN; n];
    let mut count = 0u32;
    for root in 0..n {
        if t.order[root] != UNSEEN {
            continue;
        }
        t.enter(graph, root as u32);
        while let Some(&(v, next)) = t.calls.last() {
            let vi = v as usize;
            if next < graph.offsets[vi + 1] {
                t.calls.last_mut().expect("a call is open").1 += 1;
                let w = graph.steps[next as usize] as usize;
                if t.order[w] == UNSEEN {
                    t.enter(graph, w as u32);
                } else if component[w] == UNSEEN {
                    // Still open: on the path or in a component not yet closed.
                    t.low[vi] = t.low[vi].min(t.order[w]);
                }
                continue;
            }
            t.calls.pop();
            if let Some(&(u, _)) = t.calls.last() {
                t.low[u as usize] = t.low[u as usize].min(t.low[vi]);
            }
            if t.low[vi] == t.order[vi] {
                loop {
                    let w = t.open.pop().expect("v is still open");
                    component[w as usize] = count;
                    if w == v {
                        break;
                    }
                }
                count += 1;
            }
        }
    }
    (component, count as usize)
}

const UNSEEN: u32 = u32::MAX;

/// The walk state of [`components`].
struct Tarjan {
    /// The order each state was entered in.
    order: Vec<u32>,
    /// The earliest entered open state each state's walk reaches.
    low: Vec<u32>,
    /// States entered whose component is not closed yet.
    open: Vec<u32>,
    /// The states being walked, each with the next of its steps to take.
    calls: Vec<(u32, u32)>,
    seen: u32,
}

impl Tarjan {
    fn enter(&mut self, graph: &Graph, v: u32) {
        self.order[v as usize] = self.seen;
        self.low[v as usize] = self.seen;
        self.seen += 1;
        self.open.push(v);
        self.calls.push((v, graph.offsets[v as usize]));
    }
}
