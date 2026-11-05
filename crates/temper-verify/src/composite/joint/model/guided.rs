//! Phase two of a joint walk: guided walks past where the breadth-first
//! walk stops.
//!
//! The breadth-first walk ([`super::walk`]) sees every reachable joint
//! state, or stops when its room says no and the rule is NOT CHECKED. A
//! break that sits deeper than the room lets it reach goes unseen. When it
//! stops, the walks here go on from the states it started from and the
//! deepest ones it reached, each with the path that reached it. Every step
//! is a move of the same model, made the same way: [`JointModel::moves`],
//! then [`JointModel::next`] for each, and one of the states they lead to,
//! chosen by a [`Guide`] ([`plain`] at install), with ties and one step in
//! four left to a seeded random number, so the same specs give the same
//! walks, verdict and trace every time.
//!
//! What it may say:
//!
//! - Never HOLDS. It sees some of the states, not all of them. When it finds
//!   nothing the rule stays NOT CHECKED, and the reason says how far it got.
//! - BROKEN for an invariant only on a real path: a settled state the rule
//!   is false in, reached from one of the model's starting states by sure
//!   moves alone, as for the breadth-first walk ([`super::walk`]). Before it
//!   is reported, the trace is replayed step by step: its first state must
//!   be one [`JointModel::inits`] makes, and each next state one that a move
//!   of the model surely leads to from the one before. A trace that does not
//!   replay is never reported as a break.
//! - NEVER SETTLES for a liveness property only where a complete walk
//!   proves it: from a settled state where the condition is false, reached
//!   by sure moves alone on a path that replays, a walk of
//!   every move the liveness check counts on (the system's own, and those
//!   outside code is relied on to send while the condition is false), to
//!   the last state they reach, decided by the same fixpoint
//!   ([`settle`]). Every state that walk reaches is reachable, and what
//!   decides whether it reaches the condition lies inside the walk, so a
//!   stuck state there is stuck in the model. A walk that does not finish
//!   within its share of steps or the room confirms nothing.
//!
//! Nothing here takes a move the model does not take, so the rule that makes
//! a pass that checked nothing NOT CHECKED (a premise nothing meets) can
//! never turn into a break here.
//!
//! Bounds, each for a reason outside this code:
//!
//! - Memory: the same room as the breadth-first walk, which is the host's
//!   ([`super::super::room`]). Everything kept here is counted against it:
//!   the fingerprints of states seen, the trace, what each move builds, and
//!   each confirming walk.
//! - Steps: [`Budget::INSTALL`], from install time; see [`STEPS`].

mod live;
mod search;

pub use search::{GuidedSearch, guided_search};

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash, Hasher};

use temper_spec::predicate::{Expr, Name, Truth};

use super::super::pack::{Packed, Packer};
use super::super::room::{Bound, Held, block, grow_map};
use super::super::state::render;
use super::super::{JointOutcome, JointState, JointStep};
use super::JointModel;
use super::held::Tables;
use super::step::{Certainty, Move};
use super::walk::Explored;

/// Expansions phase two makes for one proof at install: each is one state's
/// moves and every state they lead to, the work the breadth-first walk does
/// for each state it walks. The bound is install time: an install proves
/// each rule once while a person waits, so phase two targets about ten
/// seconds for each rule the breadth-first walk could not finish. Measured
/// (Apple M3 Max, release build, on a shared machine): on the joint model of
/// two types of a 35-type app, 20,000 expansions took 5.7 to 6.9 s, about
/// 0.3 ms each; on the generated specs of the tests, 1 to 2.5 s.
pub const STEPS: usize = 20_000;

/// How much work phase two may do.
#[derive(Debug, Clone, Copy)]
pub(in crate::composite::joint) struct Budget {
    /// Expansions in all, walks and confirming walks together.
    pub(in crate::composite::joint) steps: usize,
    /// Expansions one confirming walk may take: an eighth of the whole, so
    /// one stuck-looking region whose own walk never ends leaves the walks
    /// most of their steps.
    pub(in crate::composite::joint) confirm: usize,
}

impl Budget {
    /// What an install spends.
    pub(in crate::composite::joint) const INSTALL: Budget = Budget {
        steps: STEPS,
        confirm: STEPS / 8,
    };
}

/// A walk ends when this many steps in a row reach no state any walk has
/// seen: the plain order takes an unseen state whenever one is a step away,
/// so a walk this long without one is circling among seen states and spends
/// its steps learning nothing. The next walk starts from another seed.
const STALE: usize = 16;

/// The breadth-first walk's deepest states walks start from, one per set of
/// record statuses, and its starting states. Their paths are copied while
/// the breadth-first walk still holds its room, before memory can be asked
/// for them, so they are few: at 64 paths a few dozen states deep, packed,
/// they take some hundreds of KB.
const SEEDS: usize = 64;
const STARTS: usize = 8;

/// How far back from its last state the breadth-first walk is searched for
/// seeds: the last few thousand states are the deepest it reached, and
/// unpacking more costs time and finds states no deeper.
const SCAN: usize = 4096;

/// One state of a trace: the move that led to it (a name in the packer;
/// `None` for a start), the state, packed, and whether sure moves alone
/// reached it along this trace.
type Step = (Option<u32>, Packed, bool);

/// What an [`Guide`] sees of one state a walk may step to. [`plain`] reads
/// only the flags; the states, the move and the depth are there for orders
/// measured against it ([`guided_search`]).
pub struct Candidate<'c> {
    /// The state the walk is in.
    pub from: &'c JointState,
    /// The state the step leads to.
    pub to: &'c JointState,
    /// The move, as the trace would name it.
    pub label: &'c str,
    /// Moves from the start to `from`.
    pub depth: usize,
    /// No walk has seen `to`.
    pub new_state: bool,
    /// No walk has seen a state with `to`'s record statuses.
    pub new_statuses: bool,
    /// The step changes a field or status the rule reads.
    pub reads_changed: bool,
}

/// How a walk orders the states it may step to: the higher the score, the
/// likelier the step. One of the best is taken three steps in four, and
/// any one at random the fourth, so a poor order still walks everywhere.
/// It is a parameter so another order (an outside model's, say) can be
/// measured offline against [`plain`] on the same specs.
pub type Guide<'o> = dyn FnMut(&Candidate<'_>) -> u32 + 'o;

/// The order an install walks by: new record statuses first (a status the
/// walks have not reached is where the breadth-first walk ran out), then
/// any new state, then a step that changes what the rule reads.
pub fn plain(candidate: &Candidate<'_>) -> u32 {
    4 * u32::from(candidate.new_statuses)
        + 2 * u32::from(candidate.new_state)
        + u32::from(candidate.reads_changed)
}

/// How far phase two got.
#[derive(Debug, Default, Clone)]
pub(in crate::composite::joint) struct Stats {
    /// Walks started.
    pub(in crate::composite::joint) walks: usize,
    /// Expansions made, walks and confirming walks together.
    pub(in crate::composite::joint) steps: usize,
    /// The most moves a trace went from its start.
    pub(in crate::composite::joint) deepest: usize,
    /// Distinct states the walks reached.
    pub(in crate::composite::joint) distinct: usize,
    /// Liveness: settled states where the condition was false whose own
    /// walk was tried, and how many of those walks did not finish.
    pub(in crate::composite::joint) candidates: usize,
    pub(in crate::composite::joint) unconfirmed: usize,
    /// The steps and distinct states when the first break was confirmed.
    pub(in crate::composite::joint) found: Option<(usize, usize)>,
    /// Why phase two stopped before spending its steps.
    pub(in crate::composite::joint) stopped: Option<String>,
    /// The kinds of move walks left untaken because what they build did
    /// not fit, in words.
    pub(in crate::composite::joint) too_big: Vec<String>,
    /// A move was held back for [`super::JointModel::in_flight`].
    pub(in crate::composite::joint) held_back: bool,
}

impl Stats {
    /// How far it got, in words, after why the breadth-first walk stopped.
    fn describe(&self, why: &str) -> String {
        let mut text = format!(
            "{why}; past it, {} guided walks made {} steps and reached {} distinct states, at most {} moves from a start, without a break",
            self.walks, self.steps, self.distinct, self.deepest
        );
        if self.candidates > 0 {
            text.push_str(&format!(
                ", and walked on from {} states where the condition was false, {} of which it could not finish",
                self.candidates, self.unconfirmed
            ));
        }
        if !self.too_big.is_empty() {
            text.push_str(&format!(
                "; it never took {}, since what each builds does not fit, and a break past them is not ruled out",
                self.too_big.join(", ")
            ));
        }
        if let Some(reason) = &self.stopped {
            text.push_str(&format!("; the guided walks stopped: {reason}"));
        }
        text
    }
}

/// What the walker does after a step.
#[derive(PartialEq, Eq)]
enum Look {
    Go,
    Stop,
}

/// A fixed hash, so a fingerprint is the same in every run of one build.
fn print<T: Hash + ?Sized>(value: &T) -> u64 {
    let mut hasher = DefaultHasher::new();
    value.hash(&mut hasher);
    hasher.finish()
}

fn statuses(state: &JointState) -> u64 {
    print(
        &state
            .records
            .values()
            .map(|r| &r.status)
            .collect::<Vec<_>>(),
    )
}

/// The fields (and `status`) of each entity a rule reads.
fn reads(owner: &str, rules: &[&Expr]) -> BTreeMap<String, BTreeSet<String>> {
    let mut read: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut add = |entity: &str, field: &str| {
        read.entry(entity.to_string())
            .or_default()
            .insert(field.to_string());
    };
    for rule in rules {
        rule.for_each_name(&mut |name| match name {
            Name::Var(field) => add(owner, field),
            Name::Status => add(owner, "status"),
            Name::CrossStatus {
                entity_type,
                id_field,
            } => {
                add(owner, id_field);
                add(entity_type, "status");
            }
            Name::CrossField {
                entity_type,
                id_field,
                field,
            } => {
                add(owner, id_field);
                add(entity_type, field);
            }
            Name::Param(_) => {}
        });
    }
    read
}

/// Whether a field or status `read` names differs between `from` and `to`.
fn changes(read: &BTreeMap<String, BTreeSet<String>>, from: &JointState, to: &JointState) -> bool {
    read.iter().any(|(entity, fields)| {
        let (Some(a), Some(b)) = (
            from.records.get(entity.as_str()),
            to.records.get(entity.as_str()),
        ) else {
            return false;
        };
        fields.iter().any(|field| {
            if field == "status" {
                return a.status != b.status;
            }
            a.counters.get(field) != b.counters.get(field)
                || a.booleans.get(field) != b.booleans.get(field)
                || a.strings.get(field) != b.strings.get(field)
                || a.lists.get(field) != b.lists.get(field)
        })
    })
}

/// A move's kind: which of the model's moves, on which entity, for which
/// action or module.
fn kind(mv: &Move) -> (u8, String, String) {
    match mv {
        Move::Take(entity, action, _) => (0, entity.to_string(), action.name.to_string()),
        Move::Send(entity, action, _) => (1, entity.to_string(), action.name.to_string()),
        Move::Timer(entity, action) => (2, entity.clone(), action.clone()),
        Move::Answer(call, action) => (3, call.to.to_string(), action.name.to_string()),
        Move::Fail(call) => (4, call.to.to_string(), call.module.to_string()),
        Move::Lost(call) => (5, call.to.to_string(), call.module.to_string()),
        Move::Read(dispatch) => (6, dispatch.to.to_string(), dispatch.action.to_string()),
        Move::Arrive(dispatch) => (7, dispatch.to.to_string(), dispatch.action.to_string()),
        Move::Reaction(entity, action, _) => (8, entity.to_string(), action.name.to_string()),
        Move::Silent(call) => (9, call.to.to_string(), call.module.to_string()),
    }
}

/// A move's kind in words.
fn words((variant, entity, name): &(u8, String, String)) -> String {
    let how = match variant {
        0 | 1 => "sent from outside",
        2 => "by its timer",
        3 => "as a module's answer",
        4 => "failing",
        5 => "lost to a restart",
        6 => "read on its dispatch path",
        7 => "arriving",
        _ => "by a reaction from outside the scope",
    };
    format!("{entity}.{name} {how}")
}

/// A small fixed-seed generator (SplitMix64): the same seed, the same walks.
struct Random(u64);

impl Random {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// The seed of walk `walk`.
fn seed(walk: usize) -> Random {
    Random(0x007E_3BE2_u64 ^ (walk as u64).wrapping_mul(0xD1B5_4A32_D192_ED03))
}

/// The guided walks over one model, and the memory they hold.
struct Engine<'m, 'a> {
    model: &'m JointModel,
    bound: Bound<'a>,
    held: Held<'a>,
    tables: Tables,
    packer: Packer,
    read: BTreeMap<String, BTreeSet<String>>,
    /// Fingerprints of the states walks reached, and of their statuses.
    seen: HashMap<u64, ()>,
    seen_statuses: HashMap<u64, ()>,
    /// Liveness: states known to reach the condition, and states whose own
    /// walk was tried.
    good: HashMap<u64, ()>,
    tried: HashMap<u64, ()>,
    /// The kinds of move ([`kind`]) whose states did not fit in the room.
    too_big: BTreeSet<(u8, String, String)>,
    /// Bytes of the packer's table counted so far.
    packer_counted: usize,
    left: usize,
    stats: Stats,
}

impl<'m, 'a> Engine<'m, 'a> {
    fn new(
        model: &'m JointModel,
        bound: Bound<'a>,
        read: BTreeMap<String, BTreeSet<String>>,
        steps: usize,
    ) -> Self {
        // As the breadth-first walk does: the room bounds what is held, and
        // the expansion has no count of its own.
        model.expansion.reset(usize::MAX, None);
        Self {
            model,
            bound,
            held: Held::new(bound),
            tables: Tables::default(),
            packer: Packer::default(),
            read,
            seen: HashMap::new(),
            seen_statuses: HashMap::new(),
            good: HashMap::new(),
            tried: HashMap::new(),
            too_big: BTreeSet::new(),
            packer_counted: 0,
            left: steps,
            stats: Stats::default(),
        }
    }

    /// Note `key` in `set`, counting its memory; whether it was new.
    fn mark(held: &mut Held<'_>, set: &mut HashMap<u64, ()>, key: u64) -> Result<bool, String> {
        if set.contains_key(&key) {
            return Ok(false);
        }
        grow_map(set, 1, held)?;
        set.insert(key, ());
        Ok(true)
    }

    /// The bytes a trace step holds.
    fn step_bytes(step: &Step) -> usize {
        size_of::<Step>() + block(4 * step.1.len())
    }

    /// One step from `state` (`depth` moves from its start): every move of
    /// the model and every state each leads to, as the breadth-first walk
    /// expands a state, then one of them by `order`. Each state is kept
    /// packed, with its score, while the rest are built; `None` when no move
    /// leads anywhere.
    fn step(
        &mut self,
        order: &mut Guide<'_>,
        random: &mut Random,
        state: &JointState,
        depth: usize,
    ) -> Result<Option<(String, JointState, Packed, Certainty)>, String> {
        let model = self.model;
        // One step in four goes anywhere, unscored, so a poor order still
        // walks everywhere.
        let anywhere = random.below(4) == 0;
        let (moves, room) =
            model.within(&mut self.held, &mut self.tables, || model.moves(state))?;
        let mut kept: Vec<(u32, Packed, String, Certainty)> = Vec::new();
        let mut bytes = 0;
        for mv in &moves {
            // A move whose states do not fit in the room is not taken: a
            // walk may leave a way out, since it claims nothing for what it
            // does not see. Building it again in a later state would cost as
            // much and most likely fail as much, so its kind is not tried
            // again.
            let kind = kind(mv);
            if self.too_big.contains(&kind) {
                continue;
            }
            let Ok(((nexts, held_back), built)) =
                model.within(&mut self.held, &mut self.tables, || model.next(state, mv))
            else {
                self.stats.too_big.push(words(&kind));
                self.too_big.insert(kind);
                continue;
            };
            self.stats.held_back |= held_back;
            for next in nexts {
                let score = if anywhere {
                    0
                } else {
                    order(&Candidate {
                        from: state,
                        to: &next.state,
                        label: &next.label,
                        depth,
                        new_state: !self.seen.contains_key(&print(&next.state)),
                        new_statuses: !self.seen_statuses.contains_key(&statuses(&next.state)),
                        reads_changed: changes(&self.read, state, &next.state),
                    })
                };
                let packed = self.packer.pack(&next.state);
                let more = size_of::<(u32, Packed, String, Certainty)>()
                    + block(4 * packed.len())
                    + block(next.label.capacity());
                self.held.add(more)?;
                bytes += more;
                kept.push((score, packed, next.label, next.certainty));
            }
            self.tables.give(&mut self.held, built);
        }
        self.tables.give(&mut self.held, room);
        self.count_packer()?;
        if kept.is_empty() {
            return Ok(None);
        }
        let best = kept.iter().map(|k| k.0).max().unwrap_or(0);
        let ties: Vec<usize> = (0..kept.len()).filter(|&i| kept[i].0 == best).collect();
        let pick = ties[random.below(ties.len())];
        let (_, packed, label, certainty) = kept.swap_remove(pick);
        drop(kept);
        self.held.sub(bytes);
        let next = self.packer.unpack(&packed);
        Ok(Some((label, next, packed, certainty)))
    }

    /// Walk from the seeds until the steps run out or `look` says stop.
    /// What the walks hold goes back to the room when the engine is dropped.
    fn run(
        &mut self,
        seeds: &[Vec<Step>],
        order: &mut Guide<'_>,
        look: &mut dyn FnMut(&mut Self, &[Step], &JointState) -> Look,
    ) -> Result<(), String> {
        // The seeds' paths, copied before the room could be asked.
        let held: usize = seeds.iter().flatten().map(Self::step_bytes).sum();
        self.held.add(held)?;
        self.count_packer()?;
        for step in seeds.iter().flatten() {
            let state = self.packer.unpack(&step.1);
            Self::mark(&mut self.held, &mut self.seen, print(&state))?;
            Self::mark(&mut self.held, &mut self.seen_statuses, statuses(&state))?;
        }
        let mut walk = 0;
        // States seen when the last round of walks (one from each seed)
        // began. A round that reaches no new state means every seed's
        // neighbourhood is spent, and more rounds would only walk among
        // states already seen.
        let mut round = usize::MAX;
        while self.left > 0 && !seeds.is_empty() {
            if walk % seeds.len() == 0 {
                if round == self.seen.len() {
                    break;
                }
                round = self.seen.len();
            }
            let mut random = seed(walk);
            let mut trace: Vec<Step> = seeds[walk % seeds.len()].clone();
            walk += 1;
            self.stats.walks += 1;
            let mut bytes: usize = trace.iter().map(Self::step_bytes).sum();
            self.held.add(bytes)?;
            let mut state = self
                .packer
                .unpack(&trace.last().expect("a seed has a state").1);
            let mut stopped = look(self, &trace, &state) == Look::Stop;
            let mut stale = 0;
            while !stopped && self.left > 0 && stale < STALE {
                self.left -= 1;
                self.stats.steps += 1;
                let Some((label, next, packed, certainty)) =
                    self.step(order, &mut random, &state, trace.len() - 1)?
                else {
                    break;
                };
                let sure = trace.last().is_some_and(|step| step.2) && certainty == Certainty::Sure;
                let step = (Some(self.packer.id(&label)), packed, sure);
                let more = Self::step_bytes(&step);
                self.held.add(more)?;
                bytes += more;
                let fresh = Self::mark(&mut self.held, &mut self.seen, print(&next))?;
                Self::mark(&mut self.held, &mut self.seen_statuses, statuses(&next))?;
                self.count_packer()?;
                stale = if fresh { 0 } else { stale + 1 };
                trace.push(step);
                state = next;
                self.stats.deepest = self.stats.deepest.max(trace.len() - 1);
                self.stats.distinct = self.seen.len();
                self.held.walked(self.stats.distinct);
                stopped = look(self, &trace, &state) == Look::Stop;
            }
            self.held.sub(bytes);
            if stopped {
                break;
            }
        }
        self.stats.distinct = self.seen.len();
        Ok(())
    }

    /// Count what the packer's table grew by since it was last counted.
    fn count_packer(&mut self) -> Result<(), String> {
        let now = self.packer.bytes();
        if now > self.packer_counted {
            self.held.add(now - self.packer_counted)?;
            self.packer_counted = now;
        }
        Ok(())
    }

    /// `trace` replayed through the model: its first state is one the model
    /// starts in, and each next one a state a move of the model leads to
    /// from the one before, named as the trace names it, and surely where
    /// the trace says so. The path, or why it does not replay.
    fn replay(&mut self, trace: &[Step]) -> Result<Vec<JointStep>, String> {
        let model = self.model;
        let first = self.packer.unpack(&trace[0].1);
        if !model.inits().any(|init| init == first) {
            return Err("its first state is not one the model starts in".into());
        }
        let mut path = vec![JointStep {
            action: None,
            state: render(&first),
            unsure: false,
        }];
        let mut state = first;
        for (at, (label, packed, sure)) in trace.iter().enumerate().skip(1) {
            let want = self.packer.unpack(packed);
            let name = label.map(|id| self.packer.name(id).to_string());
            let (moves, room) =
                model.within(&mut self.held, &mut self.tables, || model.moves(&state))?;
            let mut found = false;
            for mv in &moves {
                // A walk never takes a move whose states did not fit.
                if self.too_big.contains(&kind(mv)) {
                    continue;
                }
                let Ok(((nexts, _), built)) =
                    model.within(&mut self.held, &mut self.tables, || model.next(&state, mv))
                else {
                    continue;
                };
                found = nexts.iter().any(|next| {
                    next.state == want
                        && Some(&next.label) == name.as_ref()
                        && (!sure || next.certainty == Certainty::Sure)
                });
                self.tables.give(&mut self.held, built);
                if found {
                    break;
                }
            }
            self.tables.give(&mut self.held, room);
            if !found {
                return Err(format!("no move of the model leads to its state {at}"));
            }
            path.push(JointStep {
                action: name,
                state: render(&want),
                unsure: false,
            });
            state = want;
        }
        Ok(path)
    }
}

/// The breadth-first walk's states guided walks start from, each with the
/// path to it, repacked in `packer`: its first few starting states, and its
/// deepest states, one per set of record statuses, deepest first.
fn seeds(model: &JointModel, explored: &Explored<'_>, packer: &mut Packer) -> Vec<Vec<Step>> {
    let count = explored.len();
    let mut chosen: Vec<usize> = Vec::new();
    let mut kinds = BTreeSet::new();
    for index in (count.saturating_sub(SCAN)..count).rev() {
        if chosen.len() == SEEDS - STARTS {
            break;
        }
        if kinds.insert(statuses(&explored.state(index))) {
            chosen.push(index);
        }
    }
    chosen.extend(
        (0..count)
            .take_while(|&index| explored.nodes[index].parent.is_none())
            .take(STARTS),
    );
    chosen
        .into_iter()
        .map(|mut index| {
            let mut path = Vec::new();
            loop {
                let node = &explored.nodes[index];
                let (parent, sure) = (node.parent, node.sure());
                let label = explored.label(model, index).map(|label| packer.id(&label));
                path.push((label, packer.pack(&explored.state(index)), sure));
                match parent {
                    Some((from, _)) => index = from as usize,
                    None => break,
                }
            }
            path.reverse();
            path
        })
        .collect()
}

/// Why phase two can say nothing: the memory it needs is gone.
fn could_not_start(why: &str, reason: &str) -> String {
    format!("{why}; the guided walks past it could not start: {reason}")
}

impl JointModel {
    /// Phase two for `invariants` of `owner`, after the breadth-first walk
    /// `explored` stopped: an invariant it left NOT CHECKED is BROKEN when a
    /// guided walk breaks it by sure moves alone on a path that replays, and
    /// otherwise stays NOT CHECKED, saying how far the walks got.
    pub(super) fn guided_invariants(
        &self,
        owner: &str,
        invariants: &[&Expr],
        outcomes: Vec<JointOutcome>,
        explored: Explored<'_>,
        bound: Bound<'_>,
    ) -> Vec<JointOutcome> {
        self.guided_invariants_by(
            owner,
            invariants,
            outcomes,
            explored,
            bound,
            (&mut plain, Budget::INSTALL),
        )
        .0
    }

    /// [`Self::guided_invariants`] by `order` within `budget`, with how far
    /// the walks got.
    pub(in crate::composite::joint) fn guided_invariants_by(
        &self,
        owner: &str,
        invariants: &[&Expr],
        mut outcomes: Vec<JointOutcome>,
        explored: Explored<'_>,
        bound: Bound<'_>,
        (order, budget): (&mut Guide<'_>, Budget),
    ) -> (Vec<JointOutcome>, Stats) {
        let Some(why) = explored.stopped.clone() else {
            return (outcomes, Stats::default());
        };
        let open: Vec<usize> = (0..outcomes.len())
            .filter(|&i| matches!(outcomes[i], JointOutcome::Incomplete { .. }))
            .collect();
        if open.is_empty() || explored.len() == 0 {
            return (outcomes, Stats::default());
        }
        let mut engine = Engine::new(self, bound, reads(owner, invariants), budget.steps);
        let seeds = seeds(self, &explored, &mut engine.packer);
        // The breadth-first walk's memory goes back to the room first.
        drop(explored);
        let mut broken: Vec<Option<Vec<JointStep>>> = vec![None; invariants.len()];
        let mut unreplayed: Vec<String> = Vec::new();
        // A rule was false where the walk went by a move it cannot be sure of.
        let mut maybe = false;
        let ran = engine.run(&seeds, order, &mut |engine, trace, state| {
            if !state.settled() {
                return Look::Go;
            }
            let sure = trace.last().is_some_and(|step| step.2);
            let found: Vec<usize> = open
                .iter()
                .copied()
                .filter(|&i| {
                    broken[i].is_none()
                        && engine.model.rule_truth(owner, invariants[i], state) == Truth::False
                })
                .collect();
            if !sure {
                maybe |= !found.is_empty();
                return Look::Go;
            }
            for i in found {
                match engine.replay(trace) {
                    Ok(path) => {
                        engine
                            .stats
                            .found
                            .get_or_insert((engine.stats.steps, engine.seen.len()));
                        broken[i] = Some(path);
                    }
                    Err(reason) => unreplayed.push(reason),
                }
            }
            if open.iter().all(|&i| broken[i].is_some()) {
                Look::Stop
            } else {
                Look::Go
            }
        });
        if let Err(reason) = ran {
            engine.stats.stopped = Some(reason);
        }
        let stats = engine.stats.clone();
        for i in open {
            match broken[i].take() {
                Some(path) => outcomes[i] = JointOutcome::Broken { path },
                None => {
                    if let JointOutcome::Incomplete { reason } = &mut outcomes[i]
                        && *reason == why
                    {
                        *reason = if stats.walks == 0 {
                            could_not_start(&why, stats.stopped.as_deref().unwrap_or("no seeds"))
                        } else {
                            let mut text = stats.describe(&why);
                            if let Some(failed) = unreplayed.first() {
                                text.push_str(&format!(
                                    "; a break it found did not replay ({failed}), so it is not reported"
                                ));
                            }
                            if maybe {
                                text.push_str(
                                    "; a rule was false on a way the runtime may never take, so it is not reported",
                                );
                            }
                            text
                        };
                    }
                }
            }
        }
        (outcomes, stats)
    }
}
