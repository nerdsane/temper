//! Invariants over several entities.
//!
//! An `[[invariant]]` may read a related entity: its status
//! (`Type[id].status`) or a field (`Type[id].field`). One entity's model
//! cannot know those values, so the per-entity cascade reads them as unknown
//! and never fails on them. They are checked here, on the joint state of the
//! invariant's entity and every type it names, one record of each, run the
//! way the runtime runs them:
//!
//! - each move is made by whoever makes it in the runtime, and the app's
//!   Cedar decides it as them: an action someone outside may send (with the
//!   actions a `[[liveness]]` in scope `relies_on`), a reaction as its
//!   declared principal or its source's sender, and the kernel's own moves
//!   (state timers, module answers and failures, module calls lost to a
//!   restart). Without policies, anyone may send anything. Liveness reads
//!   the same senders (`senders.rs`);
//! - each record has an id, and a reference finds a record only when its id
//!   field holds that record's id; walks start from every way the records'
//!   ids can be equal or differ, so a design that works only when two ids
//!   happen to match is found;
//! - an action whose guard reads a related record (`Type[id].status`)
//!   reads it on the dispatch path and is written later by its record's
//!   actor, which checks only its own state; other moves can land in
//!   between;
//! - an entity reaction is a dispatch of its own, sent after its source
//!   commits with `args` copied from the source as committed, and taken by
//!   its target later, in any order with other dispatches;
//! - a reaction from a type outside the scope is a move from outside: it
//!   comes whenever its target's guard may let it, with any parameters, as
//!   a sender Cedar may answer either way for, in a chain that may already
//!   hold any step that can come before it;
//! - string variables are tracked as symbols (equality only) when an
//!   invariant reads them, when a reference uses them as an id, or when
//!   their value reaches one that is read
//!   ([`crate::model::build_model_tracking`]);
//! - each entity is sliced to what the invariants depend on: a counter,
//!   boolean or list nothing read can be influenced by is dropped, and a
//!   guard that read it becomes a free choice. That only adds behaviour, so
//!   an invariant that holds here holds in the full model; a broken one
//!   comes with the path that breaks it, to read against the spec.
//!
//! Records disagree while a dispatch is on its way, so invariants are
//! checked in settled states: those with nothing in flight. The walk is
//! breadth first, so each broken invariant's path is a shortest one. A walk
//! that does not fit in the memory its [`Room`] gives it says so and is
//! never a pass. Guided walks then go on past where it stopped, by the same
//! moves (`model/guided.rs`): they can find a break it did not reach, with
//! a path that replays, and never make a pass either.
//!
//! Nor is a walk that checked nothing. A module whose trigger names no
//! `on_success` may answer with any action its result names, and the model
//! answers only with internal ones: when such a call is open and its record
//! could take an action nothing in the model sends, the walk never goes
//! where that answer leads, and says so instead of passing. A rule written
//! `premise => conclusion` whose premise no walked state meets (no settled
//! one, for an invariant) holds without being tested, and proves nothing.
//!
//! The drop check (ADR-0150, [`dropped_reactions`]) walks the same model over
//! a whole trigger-graph component, unsliced, one record of each type that
//! every reference finds, with a move's dispatches landing before the next
//! move from outside, and names each reaction a target refuses.

#[macro_use]
mod pair;

mod env;
mod fair;
mod liveness;
mod model;
mod pack;
mod proof;
mod replay;
mod room;
mod senders;
mod slice;
mod state;

pub use fair::{FairOutcome, FairVerdict, Lasso};
pub use liveness::{JointLivenessResult, LivenessOutcome, StuckCase, verify_joint_liveness};
pub use proof::{
    InvariantProof, JointProof, LivenessProof, invariant_proofs, invariant_proofs_holding,
    joint_proofs, liveness_proofs,
};
pub use replay::{JointReplay, TraceMove, replay_joint_invariants};
pub use room::{Bound, Bytes, Progress, Room};

use std::fmt;

use temper_authz::SendAnalysis;
use temper_spec::automaton::Automaton;
use temper_spec::predicate::Expr;

pub use model::DroppedReaction;
pub(crate) use model::dropped_reactions;
pub use model::{Candidate, GUIDED_STEPS, Guide, GuidedSearch, guided_search, plain};
pub use state::{Call, Committed, Dispatch, JointState, ReactionStep};

#[cfg(test)]
use model::{JointModel, Sending};
#[cfg(test)]
use std::collections::{BTreeMap, BTreeSet};

/// What the joint check found for one invariant.
#[derive(Debug, Clone)]
pub struct JointInvariantResult {
    /// The entity type that declares the invariant.
    pub entity: String,
    /// The invariant's name.
    pub invariant: String,
    /// Its assertion, as written.
    pub assert: String,
    /// The entity types composed to check it.
    pub scope: Vec<String>,
    /// Distinct joint states walked.
    pub states: usize,
    /// How many records of each type the walk held: what it found holds for
    /// up to that many.
    pub records: usize,
    /// When the walk held a move back because it would have put more than
    /// this many dispatches in flight: what it found holds only up to that.
    /// A commit whose reactions wait for their guards to be read counts as
    /// one, however many wait.
    pub in_flight_bound: Option<usize>,
    /// Holds, broken (with a shortest path), or not fully checked.
    pub outcome: JointOutcome,
}

impl JointInvariantResult {
    /// Whether the invariant was checked over every reachable joint state and
    /// held in all of them.
    pub fn holds(&self) -> bool {
        matches!(self.outcome, JointOutcome::Holds)
    }
}

/// The outcome of checking one invariant on the joint model.
#[derive(Debug, Clone)]
pub enum JointOutcome {
    /// It held in every reachable joint state.
    Holds,
    /// A reachable joint state breaks it. `path` starts at the initial joint
    /// state; each later step names the move that led to it.
    Broken {
        /// The moves and the joint states they reach.
        path: Vec<JointStep>,
    },
    /// No settled state the walk met breaks it, but none meets its premise
    /// either, so that proves nothing.
    Vacuous {
        /// The premise, as written.
        premise: String,
    },
    /// The walk stopped before every state was seen, never took an answer
    /// a module may give, or could not start.
    Incomplete {
        /// Why.
        reason: String,
    },
}

/// The premise of a rule written `premise => conclusion`.
fn premise(rule: &Expr) -> Option<&Expr> {
    match rule {
        Expr::Implies(premise, _) => Some(premise),
        _ => None,
    }
}

/// One step of a counterexample path.
#[derive(Debug, Clone)]
pub struct JointStep {
    /// The move, `Entity.Action(params) -> Status`; `None` for the start.
    pub action: Option<String>,
    /// The joint state after it.
    pub state: String,
    /// The move rests on what the model cannot decide: a Cedar answer, a
    /// guard, a reference or a reaction from outside the scope. The verdict
    /// holds if the runtime can make it.
    pub unsure: bool,
}

/// The moves `path` rests on, as a condition on a verdict; empty when sure
/// moves alone make it.
pub(super) fn condition(path: &[JointStep]) -> String {
    let moves: Vec<String> = path
        .iter()
        .filter(|step| step.unsure)
        .filter_map(|step| step.action.as_ref())
        .map(|action| format!("`{action}`"))
        .collect();
    if moves.is_empty() {
        String::new()
    } else {
        format!(" if the runtime can make {}", moves.join(" and "))
    }
}

impl fmt::Display for JointInvariantResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scope = self.scope.join(" + ");
        match &self.outcome {
            JointOutcome::Holds => {
                let upto = match self.records {
                    1 => String::new(),
                    records => format!(" for up to {records} records of each type,"),
                };
                write!(
                    f,
                    "{}.{} HOLDS{upto} over {} joint states ({scope}",
                    self.entity, self.invariant, self.states
                )?;
                match self.in_flight_bound {
                    Some(bound) => write!(f, "; with at most {bound} dispatches in flight)"),
                    None => f.write_str(")"),
                }
            }
            JointOutcome::Broken { path } => {
                writeln!(
                    f,
                    "{}.{} BROKEN{} after {} moves ({scope}, {} joint states walked): {}",
                    self.entity,
                    self.invariant,
                    condition(path),
                    path.len().saturating_sub(1),
                    self.states,
                    self.assert
                )?;
                if self.records > 1 {
                    writeln!(
                        f,
                        "  with {} records of each type. The walk keeps them in one order, so `Type#2` can be another record from one step to the next",
                        self.records
                    )?;
                }
                for step in path {
                    match &step.action {
                        Some(action) => writeln!(f, "  {action}\n      {}", step.state)?,
                        None => writeln!(f, "  start\n      {}", step.state)?,
                    }
                }
                Ok(())
            }
            JointOutcome::Vacuous { premise } => write!(
                f,
                "{}.{} CHECKS NOTHING AS WRITTEN: its premise `{premise}` holds in no settled state of the {} joint states walked ({scope})",
                self.entity, self.invariant, self.states
            ),
            JointOutcome::Incomplete { reason } => write!(
                f,
                "{}.{} NOT CHECKED ({scope}, {} joint states walked): {reason}",
                self.entity, self.invariant, self.states
            ),
        }
    }
}

/// Check every `[[invariant]]` in `automatons` that reads a related entity,
/// on the joint model of its entity and the types it names
/// ([`invariant_proofs`]).
pub fn verify_joint_invariants(
    automatons: &[&Automaton],
    policies: Option<&SendAnalysis>,
    bound: Bound<'_>,
) -> Vec<JointInvariantResult> {
    invariant_proofs(automatons, policies, bound.room)
        .iter()
        .flat_map(|proof| proof.check(bound))
        .collect()
}

#[cfg(test)]
#[path = "joint_test.rs"]
mod tests;

#[cfg(test)]
#[path = "vacuous_test.rs"]
mod vacuous_tests;

#[cfg(test)]
#[path = "both_ways_test.rs"]
mod both_ways_tests;

#[cfg(test)]
mod chain_test;
#[cfg(test)]
mod copies_test;

#[cfg(test)]
#[path = "literals_test.rs"]
mod literals_tests;

#[cfg(test)]
#[path = "guided_test.rs"]
mod guided_tests;

#[cfg(test)]
#[path = "sure_test.rs"]
mod sure_tests;

#[cfg(test)]
#[path = "someone_test.rs"]
mod someone_tests;

#[cfg(test)]
#[path = "classes_test.rs"]
mod classes_tests;

#[cfg(test)]
#[path = "outside_test.rs"]
mod outside_tests;

#[cfg(test)]
#[path = "idle_test.rs"]
mod idle_tests;

#[cfg(test)]
#[path = "blank_test.rs"]
mod blank_tests;

#[cfg(test)]
#[path = "outside_count_test.rs"]
mod outside_count_tests;

#[cfg(test)]
#[path = "widen_test.rs"]
mod widen_tests;

#[cfg(test)]
#[path = "trigger_guard_test.rs"]
mod trigger_guard_tests;

#[cfg(test)]
#[path = "trigger_order_test.rs"]
mod trigger_order_tests;
