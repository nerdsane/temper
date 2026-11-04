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

mod env;
mod fair;
mod liveness;
mod model;
mod pack;
mod proof;
mod room;
mod senders;
mod slice;
mod state;

pub use fair::{FairOutcome, FairVerdict, Lasso};
pub use liveness::{JointLivenessResult, LivenessOutcome, StuckCase, verify_joint_liveness};
pub use proof::{
    InvariantProof, JointProof, LivenessProof, invariant_proofs, joint_proofs, liveness_proofs,
};
pub use room::{Bound, Bytes, Progress, Room};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use temper_authz::SendAnalysis;
use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Expr, Truth, eval};

use crate::model::{ModelString, ParamValue};

pub use model::DroppedReaction;
pub(crate) use model::dropped_reactions;
pub use model::{Candidate, GUIDED_STEPS, Guide, GuidedSearch, guided_search, plain};
use model::{JointModel, Move, Sending};
use state::render;
pub use state::{Call, Committed, Dispatch, JointState, ReactionStep};

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
}

impl fmt::Display for JointInvariantResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scope = self.scope.join(" + ");
        match &self.outcome {
            JointOutcome::Holds => {
                write!(
                    f,
                    "{}.{} HOLDS over {} joint states ({scope}",
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
                    "{}.{} BROKEN after {} moves ({scope}, {} joint states walked): {}",
                    self.entity,
                    self.invariant,
                    path.len().saturating_sub(1),
                    self.states,
                    self.assert
                )?;
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

/// One move of a trace: an entity's action with the string parameters it
/// was sent. Values are names for this trace only: equal names are equal
/// values, `''` is the empty string, and a name not seen before is a value
/// nothing holds yet. A parameter the model does not track is not checked.
#[derive(Debug, Clone)]
pub struct TraceMove {
    /// The entity type.
    pub entity: String,
    /// The action.
    pub action: String,
    /// String parameters by name; parameters left out may take any value.
    pub params: BTreeMap<String, String>,
}

/// A trace walked through the joint model of `entity`'s cross-entity
/// invariants.
#[derive(Debug, Clone)]
pub struct JointReplay {
    /// The start and the state after each step.
    pub steps: Vec<JointStep>,
    /// The invariants false in the last state.
    pub broken: Vec<String>,
}

/// Walk `trace` through the joint model of `entity`'s invariants that read
/// related entities, as the check does: whether the spec allows each move,
/// and which invariants the last state breaks. `ids` names each record's id
/// in the trace's names (equal names, equal ids); a type left out gets an
/// id of its own. A move the trace names takes a dispatch in flight to that
/// action when there is one, and is otherwise sent from outside; when it is
/// not allowed yet, dispatches in flight land first, oldest first. A
/// reaction whose guard the model cannot decide is taken as not firing. Use
/// it to ask whether what happened in production is a path of the spec.
pub fn replay_joint_invariants(
    automatons: &[&Automaton],
    entity: &str,
    ids: &BTreeMap<String, String>,
    trace: &[TraceMove],
) -> Result<JointReplay, String> {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    let automaton = by_name
        .get(entity)
        .ok_or_else(|| format!("no entity type {entity}"))?;
    let joint: Vec<&temper_spec::automaton::Invariant> = automaton
        .invariants
        .iter()
        .filter(|inv| !inv.assert.related_types().is_empty())
        .collect();
    let mut scope = BTreeSet::from([entity.to_string()]);
    for inv in &joint {
        for (entity_type, _) in inv.assert.related_types() {
            if !by_name.contains_key(entity_type.as_str()) {
                return Err(format!(
                    "{entity} reads {entity_type}, which is not declared"
                ));
            }
            scope.insert(entity_type);
        }
    }
    let exprs: Vec<&Expr> = joint.iter().map(|inv| &inv.assert).collect();
    let model = JointModel::new(&by_name, &scope, entity, &exprs, Sending::anyone(), true);
    let (mut state, mut names) = model.init_with_ids(ids);
    let mut steps = vec![JointStep {
        action: None,
        state: render(&state),
    }];
    for (i, wanted) in trace.iter().enumerate() {
        loop {
            let fits = |params: &BTreeMap<String, ParamValue>| {
                let mut bindings = names.clone();
                let mut used = model.values(&state);
                wanted.params.iter().all(|(param, name)| {
                    // A parameter the model does not track takes any value.
                    let Some(ParamValue::Str(value)) = params.get(param) else {
                        return true;
                    };
                    match bindings.get(name) {
                        Some(bound) => value == bound,
                        None if name.is_empty() => value.is_empty(),
                        None if value.symbol_id().is_some() && !used.contains(value) => {
                            bindings.insert(name.clone(), value.clone());
                            used.insert(value.clone());
                            true
                        }
                        None => false,
                    }
                })
            };
            let wanted_dispatch = |d: &Dispatch| {
                d.to == wanted.entity && d.action == wanted.action && fits(&d.params)
            };
            let moves = model.moves(&state);
            let named = moves
                .iter()
                .find(|mv| match mv {
                    Move::Read(d) | Move::Arrive(d) => wanted_dispatch(d),
                    _ => false,
                })
                .or_else(|| {
                    moves.iter().find(|mv| match mv {
                        Move::Take(e, action, _) | Move::Send(e, action, _) => {
                            e == &wanted.entity
                                && action.name == wanted.action
                                && fits(&action.params)
                        }
                        _ => false,
                    })
                });
            let (mv, done) = match named {
                Some(mv @ (Move::Take(..) | Move::Arrive(_))) => (mv.clone(), true),
                Some(mv) => (mv.clone(), false),
                None => match moves
                    .iter()
                    .find(|mv| matches!(mv, Move::Read(_) | Move::Arrive(_)))
                {
                    Some(mv) => (mv.clone(), false),
                    None => {
                        return Err(format!(
                            "move {} ({}.{}) is not allowed in {}",
                            i + 1,
                            wanted.entity,
                            wanted.action,
                            render(&state)
                        ));
                    }
                },
            };
            if let Move::Take(_, action, _) | Move::Send(_, action, _) = &mv {
                for (param, name) in &wanted.params {
                    if let Some(ParamValue::Str(value)) = action.params.get(param)
                        && !name.is_empty()
                    {
                        names.entry(name.clone()).or_insert_with(|| value.clone());
                    }
                }
            }
            let next = model
                .next(&state, &mv)
                .0
                .into_iter()
                .next()
                .ok_or_else(|| {
                    format!(
                        "move {} ({}.{}) cannot be taken",
                        i + 1,
                        wanted.entity,
                        wanted.action
                    )
                })?;
            names = names
                .into_iter()
                .filter_map(|(name, value)| match next.renamed.get(&value) {
                    Some(new) => Some((name, new.clone())),
                    None if matches!(value, ModelString::Literal(_)) => Some((name, value)),
                    None => None,
                })
                .collect();
            steps.push(JointStep {
                action: Some(next.label),
                state: render(&next.state),
            });
            state = next.state;
            if done {
                break;
            }
        }
    }
    let env = model.env(entity, &state);
    let broken = joint
        .iter()
        .filter(|inv| eval(&inv.assert, &env) == Truth::False)
        .map(|inv| inv.name.clone())
        .collect();
    Ok(JointReplay { steps, broken })
}

#[cfg(test)]
#[path = "joint_test.rs"]
mod tests;

#[cfg(test)]
#[path = "vacuous_test.rs"]
mod vacuous_tests;

#[cfg(test)]
mod chain_test;

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
#[path = "outside_test.rs"]
mod outside_tests;

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
