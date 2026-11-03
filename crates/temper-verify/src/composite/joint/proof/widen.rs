//! Walking an invariant once more with the types reactions from outside its
//! scope come from.

use std::collections::{BTreeMap, BTreeSet};

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, Invariant};
use temper_spec::predicate::Expr;

use super::super::model::{self, JointModel};
use super::super::room::{Bound, Construction, Room};
use super::super::{JointInvariantResult, JointOutcome};
use super::{InvariantProof, invariant_model};

/// The joint model of a proof's invariants with the types whose reactions
/// reach its scope from outside taken in too. The proof holds it, so its
/// digest covers what the second walk reads, and its room pays for it.
#[derive(Debug)]
pub(super) struct Wider<'room> {
    scope: BTreeSet<String>,
    model: JointModel,
    // Declared last so the model releases its allocations before its room.
    _memory: Construction<'room>,
}

impl<'room> Wider<'room> {
    /// The wider model for `invariants` of `name`, whose scope is `scope`
    /// and whose model `outside` reaches from outside: none when nothing
    /// does, or when it does not fit in `room`, and then no break walks
    /// again.
    pub(super) fn new(
        by_name: &BTreeMap<&str, &Automaton>,
        scope: &BTreeSet<String>,
        outside: BTreeSet<String>,
        name: &str,
        invariants: &[Invariant],
        policies: Option<&SendAnalysis>,
        room: &'room dyn Room,
    ) -> Option<Self> {
        if outside.is_empty() {
            return None;
        }
        let scope: BTreeSet<String> = scope.union(&outside).cloned().collect();
        let memory = Construction::new(room);
        let model = invariant_model(by_name, &scope, name, invariants, policies, &memory).ok()?;
        Some(Self {
            scope,
            model,
            _memory: memory,
        })
    }

    /// Walk once more for each invariant false only where the first walk
    /// went by a reaction from types outside its scope (`outside`, one set
    /// per invariant, empty for the rest). The first walk sent such a
    /// reaction with any parameters as any sender, so it could not be sure
    /// of it. Here it comes from its own source: a break sure moves alone
    /// reach is one the runtime can reach, so it is broken. Anything else
    /// stays not checked, naming the types: this walk holds one record of
    /// each type, and a reaction from beyond it is again unsure.
    pub(super) fn walk(
        &self,
        proof: &InvariantProof<'_>,
        outside: &[BTreeSet<String>],
        results: &mut [JointInvariantResult],
        bound: Bound<'_>,
    ) {
        let exprs: Vec<&Expr> = proof.invariants.iter().map(|inv| &inv.assert).collect();
        let walk = self.model.walk(&proof.entity, &exprs, bound);
        for ((found, from), outcome) in results.iter_mut().zip(outside).zip(&walk.outcomes) {
            if from.is_empty() {
                continue;
            }
            let JointOutcome::Incomplete { reason } = &found.outcome else {
                continue;
            };
            let why = match outcome {
                JointOutcome::Broken { path } => {
                    found.scope = self.scope.iter().cloned().collect();
                    found.states = walk.states;
                    found.in_flight_bound = walk.held_back.then_some(model::MAX_IN_FLIGHT);
                    found.outcome = JointOutcome::Broken { path: path.clone() };
                    continue;
                }
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
