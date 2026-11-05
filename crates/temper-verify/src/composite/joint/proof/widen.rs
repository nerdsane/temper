//! Walking an invariant once more with the types reactions from outside its
//! scope come from.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use temper_authz::SendAnalysis;
use temper_spec::automaton::Automaton;
use temper_spec::predicate::Expr;

use super::super::model;
use super::super::room::{Bound, Construction, Room};
use super::super::{JointInvariantResult, JointOutcome};
use super::{InvariantProof, invariant_model};

/// The app as a proof's wider model would read it, kept to build that model
/// if a walk needs it: most walks never do, and most of what building a model
/// costs is asking Cedar about its senders. The proof's digest covers what
/// the model would read through `reads`. That is the whole of each type in
/// the scope, so a change a cut would leave out walks the proof once more too.
pub(super) struct Wider<'room> {
    /// The scope the first walk had.
    first: BTreeSet<String>,
    scope: BTreeSet<String>,
    app: Arc<BTreeMap<String, Automaton>>,
    policies: Option<SendAnalysis>,
    reads: String,
    room: &'room dyn Room,
    records: usize,
}

impl fmt::Debug for Wider<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wider")
            .field("first", &self.first)
            .field("scope", &self.scope)
            .field("reads", &self.reads)
            .field("records", &self.records)
            .finish_non_exhaustive()
    }
}

/// The app's automatons by name, as a model reads them.
fn named(app: &BTreeMap<String, Automaton>) -> BTreeMap<&str, &Automaton> {
    app.iter().map(|(name, a)| (name.as_str(), a)).collect()
}

/// All a model of `scope` reads of the app and of Cedar, as text: its types,
/// the reactions that reach them from beyond it, and the policies that fit an
/// action of its types, since no others can answer about one.
fn reads(
    app: &BTreeMap<String, Automaton>,
    scope: &BTreeSet<String>,
    policies: Option<&SendAnalysis>,
) -> String {
    let by_name = named(app);
    let mut text = model::reads_beyond(&by_name, scope);
    for entity in scope {
        let automaton = by_name[entity.as_str()];
        text.push_str(&format!("{automaton:?}"));
        let Some(policies) = policies else { continue };
        for action in &automaton.actions {
            let fitting = policies.fitting(&action.name, entity);
            text.push_str(&format!("{fitting:?}"));
        }
    }
    if let Some(policies) = policies {
        text.push_str(&format!("{}", policies.tells_timer_chains()));
    }
    text
}

impl<'room> Wider<'room> {
    /// What a wider model for a proof with `scope`, whose model `outside`
    /// reaches from outside, would read: nothing when nothing does.
    pub(super) fn new(
        app: &Arc<BTreeMap<String, Automaton>>,
        scope: &BTreeSet<String>,
        outside: BTreeSet<String>,
        policies: Option<&SendAnalysis>,
        room: &'room dyn Room,
        records: usize,
    ) -> Option<Self> {
        if outside.is_empty() {
            return None;
        }
        let first = scope.clone();
        let scope: BTreeSet<String> = scope.union(&outside).cloned().collect();
        let reads = reads(app, &scope, policies);
        Some(Self {
            first,
            scope,
            app: Arc::clone(app),
            policies: policies.cloned(),
            reads,
            room,
            records,
        })
    }

    /// Walk once more for each invariant false only where the first walk
    /// went by a reaction from types outside its scope (`outside`, one set
    /// per invariant, empty for the rest). The first walk sent such a
    /// reaction with any parameters as any sender, so it could not be sure
    /// of it. Here it comes from its own source: a break sure moves alone
    /// reach is one the runtime can reach, so it is broken. Anything else
    /// stays not checked, naming the types: a reaction from beyond this
    /// wider model is again unsure. When the
    /// model does not fit in the room, no break walks again.
    ///
    /// The sources on the ways the first walk found come first, alone: a
    /// break there is found without the state of every other source. Only
    /// what they leave undecided walks with all of them.
    pub(super) fn walk(
        &self,
        proof: &InvariantProof<'_>,
        outside: &[BTreeSet<String>],
        results: &mut [JointInvariantResult],
        bound: Bound<'_>,
    ) {
        let on_the_way: BTreeSet<String> = self
            .first
            .iter()
            .chain(outside.iter().flatten())
            .cloned()
            .collect();
        if on_the_way != self.scope {
            self.walk_with(&on_the_way, proof, outside, results, bound);
        }
        self.walk_with(&self.scope, proof, outside, results, bound);
    }

    /// Walk `scope`. Only with the whole scope does a walk that finds no
    /// break say so: with less, a break may yet come from the types left out.
    fn walk_with(
        &self,
        scope: &BTreeSet<String>,
        proof: &InvariantProof<'_>,
        outside: &[BTreeSet<String>],
        results: &mut [JointInvariantResult],
        bound: Bound<'_>,
    ) {
        let undecided = |(found, from): (&JointInvariantResult, &BTreeSet<String>)| {
            !from.is_empty() && matches!(found.outcome, JointOutcome::Incomplete { .. })
        };
        if !results.iter().zip(outside).any(undecided) {
            return;
        }
        // Declared first so the model releases its allocations before its room.
        let memory = Construction::new(self.room);
        let Ok(model) = invariant_model(
            &named(&self.app),
            scope,
            &proof.entity,
            &proof.invariants,
            self.policies.as_ref(),
            &memory,
            self.records,
        ) else {
            return;
        };
        let exprs: Vec<&Expr> = proof.invariants.iter().map(|inv| &inv.assert).collect();
        let walk = model.walk(&proof.entity, &exprs, bound);
        for ((found, from), outcome) in results.iter_mut().zip(outside).zip(&walk.outcomes) {
            if from.is_empty() {
                continue;
            }
            let JointOutcome::Incomplete { reason } = &found.outcome else {
                continue;
            };
            let why = match outcome {
                JointOutcome::Broken { path } => {
                    found.scope = scope.iter().cloned().collect();
                    found.states = walk.states;
                    found.in_flight_bound = walk.held_back.then_some(model::MAX_IN_FLIGHT);
                    found.outcome = JointOutcome::Broken { path: path.clone() };
                    continue;
                }
                _ if *scope != self.scope => continue,
                JointOutcome::Incomplete { reason } => {
                    format!("{} joint states walked, not checked: {reason}", walk.states)
                }
                JointOutcome::Holds | JointOutcome::Vacuous { .. } => format!(
                    "{} joint states walked, no break that sure moves alone reach",
                    walk.states
                ),
            };
            let types = from.iter().cloned().collect::<Vec<_>>().join(", ");
            found.outcome = JointOutcome::Incomplete {
                reason: format!("{reason}; walked again with {types} in the scope too ({why})"),
            };
        }
    }
}
