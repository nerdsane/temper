//! The search for a fair cycle: strongly connected components of the found
//! graph, each judged against the fairness constraints.

use std::collections::BTreeMap;

use temper_spec::automaton::Fairness;

use super::super::model::{Edge, MoveKind};
use super::graph::Graph;

/// One fairness constraint: a set of moves that is taken in the end.
#[derive(Debug, Clone, Copy)]
pub(super) struct Constraint {
    /// Strong rather than weak.
    pub(super) strong: bool,
    /// It holds only where the rule's condition is false: `relies_on`.
    only_failing: bool,
    /// It binds only a BROKEN verdict. Nothing says a reaction from outside the
    /// scope comes, so HOLDS may not count on it, and BROKEN may not count on
    /// its staying away.
    only_must: bool,
}

/// The fairness constraints of one rule, and which moves each one covers.
pub(super) struct Rules {
    pub(super) constraints: Vec<Constraint>,
    /// By [`Graph::keys`] index: what the runtime owes, then what the
    /// rule names.
    of_key: Vec<[Option<u32>; 2]>,
    /// By [`Graph::keys`] index: the timer a move starts again.
    resets: Vec<Option<u32>>,
    /// By [`Graph::keys`] index: its way that stays put still happened.
    stays_taken: Vec<bool>,
}

impl Rules {
    /// The constraints the module's documentation lists, for the rule of
    /// `owner` that assumes `fairness`.
    pub(super) fn new(graph: &Graph, owner: &str, fairness: &Fairness) -> Self {
        let mut relied = vec![false; graph.keys.len()];
        for (row, made) in graph.explored.made.iter().enumerate() {
            if made.relied {
                relied[graph.key_of(row as u32) as usize] = true;
            }
        }
        let mut constraints = Vec::new();
        let mut owed: BTreeMap<(MoveKind, &str, &str, &str), u32> = BTreeMap::new();
        let mut named: BTreeMap<&str, u32> = BTreeMap::new();
        let mut add = |constraint| {
            constraints.push(constraint);
            constraints.len() as u32 - 1
        };
        let weak = |only_failing, only_must| Constraint {
            strong: false,
            only_failing,
            only_must,
        };
        let of_key = graph
            .keys
            .iter()
            .enumerate()
            .map(|(index, key)| {
                let group = match key.kind {
                    MoveKind::Timer | MoveKind::Delivery | MoveKind::OutsideReaction => {
                        Some((key.action.as_str(), key.instance.as_str()))
                    }
                    // Its answers, its failure and no answer are one move: the
                    // call ends.
                    MoveKind::Call => Some(("", key.instance.as_str())),
                    MoveKind::Outside if relied[index] => Some((key.action.as_str(), "")),
                    MoveKind::Outside | MoveKind::Lost => None,
                };
                let runtime = group.map(|(action, instance)| {
                    *owed
                        .entry((key.kind, key.entity.as_str(), action, instance))
                        .or_insert_with(|| {
                            add(weak(
                                key.kind == MoveKind::Outside,
                                key.kind == MoveKind::OutsideReaction,
                            ))
                        })
                });
                let strong = fairness.strong.contains(&key.action);
                let rule = (key.entity == owner && (strong || fairness.weak.contains(&key.action)))
                    .then(|| {
                        *named.entry(key.action.as_str()).or_insert_with(|| {
                            add(Constraint {
                                strong,
                                only_failing: false,
                                only_must: false,
                            })
                        })
                    });
                [runtime, rule]
            })
            .collect();
        let resets = graph
            .keys
            .iter()
            .map(|key| {
                let (action, state) = key.resets.as_ref()?;
                Some(
                    *owed
                        .entry((MoveKind::Timer, key.entity.as_str(), action, state))
                        .or_insert_with(|| add(weak(false, false))),
                )
            })
            .collect();
        let stays_taken = graph
            .keys
            .iter()
            .map(|key| key.kind == MoveKind::Call || key.resets.is_some())
            .collect();
        Self {
            constraints,
            of_key,
            resets,
            stays_taken,
        }
    }

    fn named(&self, key: u32) -> bool {
        self.of_key[key as usize][1].is_some()
    }
}

/// Which side of a verdict a search stands on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Side {
    /// For HOLDS: every way that may be taken, and constraints only on moves
    /// sure to be possible.
    May,
    /// For BROKEN: only sure ways, and constraints on every move that may be
    /// possible.
    Must,
}

/// One search's reading of the graph.
pub(super) struct View<'a> {
    pub(super) graph: &'a Graph<'a>,
    pub(super) rules: &'a Rules,
    pub(super) failing: &'a [bool],
    pub(super) side: Side,
    /// Requests from outside and restarts may go on forever too.
    pub(super) people: bool,
    /// The states a cycle may hold.
    pub(super) open: &'a [bool],
}

/// What judging a component found.
enum Judged {
    /// A cycle through all of it is fair.
    Fair,
    /// No cycle inside it is fair.
    Never,
    /// A fair cycle inside it holds none of these states.
    Without(Vec<u32>),
}

impl View<'_> {
    /// Whether a behaviour on this side may make the move `row`.
    pub(super) fn usable(&self, row: u32) -> bool {
        let made = &self.graph.explored.made[row as usize];
        if self.people {
            return self.side == Side::May || !made.outside;
        }
        made.by_system
            || (made.relied && self.failing[made.from as usize])
            || self.rules.named(self.graph.key_of(row))
            || (self.side == Side::May && made.outside)
    }

    /// Whether a behaviour on this side may go by this way of `row`.
    pub(super) fn usable_edge(&self, row: u32, edge: &Edge) -> bool {
        let from = self.graph.explored.made[row as usize].from;
        edge.to != from
            && self.open[edge.to as usize]
            && self.usable(row)
            && (self.side == Side::May || edge.sure)
    }

    /// Whether going by this way of `row` counts as making the move. A way that
    /// stays in the same joint state usually is no move at all, but two are real
    /// events the joint state cannot show: a call that ended and started again
    /// (it has no call number), and a `reset_on` move that started a timer's
    /// clock again (it has no deadline).
    pub(super) fn taken(&self, row: u32, edge: &Edge) -> bool {
        if self.usable_edge(row, edge) {
            return true;
        }
        let from = self.graph.explored.made[row as usize].from;
        edge.to == from
            && self.open[from as usize]
            && self.usable(row)
            && (self.side == Side::May || edge.sure)
            && self.rules.stays_taken[self.graph.key_of(row) as usize]
    }

    /// The timer the move `row` starts again, which it counts as discharging:
    /// nothing owes a timer whose clock keeps starting over.
    pub(super) fn timer_reset_by(&self, row: u32) -> Option<u32> {
        self.rules.resets[self.graph.key_of(row) as usize]
    }

    /// Whether the move `row` counts as possible on this side.
    pub(super) fn possible(&self, row: u32) -> bool {
        let made = &self.graph.explored.made[row as usize];
        let moves = |edge: &Edge| edge.to != made.from;
        let mut ways = self.graph.edges_of(row).map(|(_, edge)| edge);
        match self.side {
            // A way that stays where it is may be all the runtime does: Cedar
            // or a guard the model cannot decide refuses it.
            Side::May => {
                ways.any(|edge| edge.sure && moves(edge))
                    || (made.covered && self.graph.edges_of(row).all(|(_, edge)| moves(edge)))
            }
            Side::Must => made.held || ways.any(moves),
        }
    }

    /// The constraints the move `row` counts toward, on this side.
    pub(super) fn constraints_of(&self, row: u32) -> impl Iterator<Item = u32> + '_ {
        let made = &self.graph.explored.made[row as usize];
        self.rules.of_key[self.graph.key_of(row) as usize]
            .iter()
            .flatten()
            .copied()
            .filter(move |&id| {
                let constraint = &self.rules.constraints[id as usize];
                (!constraint.only_must || self.side == Side::Must)
                    && (!constraint.only_failing || self.failing[made.from as usize])
            })
    }

    /// A component of the found graph holding a fair cycle that never leaves
    /// the open states, if there is one.
    pub(super) fn fair_cycle(&self) -> Option<Vec<u32>> {
        let count = self.graph.explored.len();
        let (starts, nexts) = self.next_states();
        let mut set_of = vec![0u32; count];
        let first: Vec<u32> = (0..count as u32)
            .filter(|&state| self.open[state as usize])
            .collect();
        for &state in &first {
            set_of[state as usize] = 1;
        }
        let mut next_set = 2u32;
        let mut work = vec![first];
        let mut tarjan = Tarjan::new(count);
        let mut scratch = Scratch::new(self.rules.constraints.len());
        while let Some(set) = work.pop() {
            for component in tarjan.components(&set, &starts, &nexts, &set_of) {
                let id = next_set;
                next_set += 1;
                for &state in &component {
                    set_of[state as usize] = id;
                }
                match self.judge(&component, &set_of, &mut scratch) {
                    Judged::Fair => return Some(component),
                    Judged::Never => {}
                    Judged::Without(removed) => {
                        for &state in &removed {
                            set_of[state as usize] = 0;
                        }
                        let rest: Vec<u32> = component
                            .into_iter()
                            .filter(|&state| set_of[state as usize] != 0)
                            .collect();
                        if !rest.is_empty() {
                            work.push(rest);
                        }
                    }
                }
            }
        }
        None
    }

    /// For each open state, the open states this side's moves lead to.
    fn next_states(&self) -> (Vec<u32>, Vec<u32>) {
        let count = self.graph.explored.len();
        let mut starts = Vec::with_capacity(count + 1);
        let mut nexts = Vec::new();
        for state in 0..count as u32 {
            starts.push(nexts.len() as u32);
            if !self.open[state as usize] {
                continue;
            }
            for &row in self.graph.rows_of(state) {
                for (_, edge) in self.graph.edges_of(row) {
                    if self.usable_edge(row, edge) {
                        nexts.push(edge.to);
                    }
                }
            }
        }
        starts.push(nexts.len() as u32);
        (starts, nexts)
    }

    /// Judge one component against every constraint a move in it counts toward.
    fn judge(&self, component: &[u32], set_of: &[u32], scratch: &mut Scratch) -> Judged {
        let id = set_of[component[0] as usize];
        scratch.clear();
        for &state in component {
            for &row in self.graph.rows_of(state) {
                let possible = self.possible(row);
                let taken = self
                    .graph
                    .edges_of(row)
                    .any(|(_, edge)| self.taken(row, edge) && set_of[edge.to as usize] == id);
                for constraint in self.constraints_of(row) {
                    scratch.note(constraint, state, possible, taken);
                }
                if let Some(timer) = self.timer_reset_by(row) {
                    scratch.note(timer, state, false, taken);
                }
            }
        }
        let mut refine = Vec::new();
        for &constraint in &scratch.touched {
            let index = constraint as usize;
            if scratch.taken[index] || scratch.possible[index] == 0 {
                continue;
            }
            if self.rules.constraints[index].strong {
                refine.push(constraint);
            } else if scratch.possible[index] as usize == component.len() {
                return Judged::Never;
            }
        }
        if refine.is_empty() {
            return Judged::Fair;
        }
        let removed = component
            .iter()
            .copied()
            .filter(|&state| {
                self.graph.rows_of(state).iter().any(|&row| {
                    self.possible(row)
                        && self
                            .constraints_of(row)
                            .any(|constraint| refine.contains(&constraint))
                })
            })
            .collect();
        Judged::Without(removed)
    }
}

/// What judging a component counts per constraint.
struct Scratch {
    possible: Vec<u32>,
    last: Vec<u32>,
    taken: Vec<bool>,
    marked: Vec<bool>,
    touched: Vec<u32>,
}

impl Scratch {
    fn new(constraints: usize) -> Self {
        Self {
            possible: vec![0; constraints],
            last: vec![u32::MAX; constraints],
            taken: vec![false; constraints],
            marked: vec![false; constraints],
            touched: Vec::new(),
        }
    }

    fn clear(&mut self) {
        for &constraint in &self.touched {
            let index = constraint as usize;
            self.possible[index] = 0;
            self.last[index] = u32::MAX;
            self.taken[index] = false;
            self.marked[index] = false;
        }
        self.touched.clear();
    }

    /// Note a move of `constraint` at `state`. States arrive one at a time, so
    /// `last` tells a new state from the one just counted.
    fn note(&mut self, constraint: u32, state: u32, possible: bool, taken: bool) {
        let index = constraint as usize;
        if !self.marked[index] {
            self.marked[index] = true;
            self.touched.push(constraint);
        }
        if possible && self.last[index] != state {
            self.last[index] = state;
            self.possible[index] += 1;
        }
        self.taken[index] |= taken;
    }
}

/// Tarjan's algorithm, on an explicit stack: walks have millions of states.
struct Tarjan {
    index: Vec<u32>,
    low: Vec<u32>,
    on_stack: Vec<bool>,
}

impl Tarjan {
    fn new(count: usize) -> Self {
        Self {
            index: vec![0; count],
            low: vec![0; count],
            on_stack: vec![false; count],
        }
    }

    /// The strongly connected components of `set`, going only by next-state
    /// pairs that stay in one set.
    fn components(
        &mut self,
        set: &[u32],
        starts: &[u32],
        nexts: &[u32],
        set_of: &[u32],
    ) -> Vec<Vec<u32>> {
        for &state in set {
            self.index[state as usize] = 0;
        }
        let mut found = Vec::new();
        let mut counter = 0u32;
        let mut stack: Vec<u32> = Vec::new();
        let mut frames: Vec<(u32, u32)> = Vec::new();
        for &root in set {
            if self.index[root as usize] != 0 {
                continue;
            }
            counter += 1;
            self.enter(root, counter, &mut stack);
            frames.push((root, starts[root as usize]));
            while let Some(frame) = frames.last_mut() {
                let state = frame.0 as usize;
                if frame.1 < starts[state + 1] {
                    let next = nexts[frame.1 as usize];
                    frame.1 += 1;
                    if set_of[next as usize] != set_of[state] {
                        continue;
                    }
                    if self.index[next as usize] == 0 {
                        counter += 1;
                        self.enter(next, counter, &mut stack);
                        frames.push((next, starts[next as usize]));
                    } else if self.on_stack[next as usize] {
                        self.low[state] = self.low[state].min(self.index[next as usize]);
                    }
                    continue;
                }
                frames.pop();
                if let Some(parent) = frames.last() {
                    let parent = parent.0 as usize;
                    self.low[parent] = self.low[parent].min(self.low[state]);
                }
                if self.low[state] == self.index[state] {
                    let mut component = Vec::new();
                    while let Some(member) = stack.pop() {
                        self.on_stack[member as usize] = false;
                        component.push(member);
                        if member as usize == state {
                            break;
                        }
                    }
                    component.sort_unstable();
                    found.push(component);
                }
            }
        }
        found
    }

    fn enter(&mut self, state: u32, counter: u32, stack: &mut Vec<u32>) {
        self.index[state as usize] = counter;
        self.low[state as usize] = counter;
        self.on_stack[state as usize] = true;
        stack.push(state);
    }
}
