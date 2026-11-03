//! The `[[invariant]]`s the kernel checks on every move.
//!
//! The verification cascade proves a spec's invariants over its model. A
//! record can still reach a state the model never reached: it was written
//! before a rule existed, under a spec that was never verified, or with values
//! the model does not track. So every move is checked as well. A move that
//! would make an invariant false, when it held before the move, is refused. An
//! invariant that did not hold before the move either is not the move's doing,
//! and does not stop it.
//!
//! A move resolves the statuses of the records it points at, never their
//! fields, so an invariant that reads another record's field is not checked
//! on moves (see [`MoveInvariant::from_spec`]).

use serde::{Deserialize, Serialize};
use temper_spec::automaton::Invariant;
use temper_spec::predicate::Expr;

use super::guard::{self, EvalContext, GuardFailure};
use super::types::TransitionTable;

/// An `[[invariant]]` the kernel checks on every move.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MoveInvariant {
    /// The invariant's name, as the spec writes it.
    pub name: String,
    /// What must hold.
    pub assert: Expr,
}

impl MoveInvariant {
    /// Whether moves check `invariant`. They cannot when it reads another
    /// record's field.
    pub fn checked(invariant: &Invariant) -> bool {
        !invariant.assert.reads_related_field()
    }

    /// The invariant as moves check it, or `None` when they cannot.
    pub fn from_spec(invariant: &Invariant) -> Option<Self> {
        Self::checked(invariant).then(|| Self {
            name: invariant.name.clone(),
            assert: invariant.assert.clone(),
        })
    }

    /// Whether a move changed which records this invariant reads the status
    /// of. The related statuses were read for the records the move started
    /// with, so they say nothing about the new ones.
    fn repointed(&self, before: &EvalContext, after: &EvalContext) -> bool {
        self.assert.cross_refs().iter().any(|(_, id_field)| {
            before.fields.get(id_field) != after.fields.get(id_field)
                || before.lists.get(id_field) != after.lists.get(id_field)
        })
    }
}

/// What a move does to the type's invariants.
#[derive(Debug, Default)]
pub struct InvariantsAcrossMove<'t> {
    /// The first invariant the move would break: it held before the move and
    /// does not after it.
    pub broken: Option<(&'t MoveInvariant, GuardFailure)>,
    /// The invariants that did not hold before the move: the record already
    /// breaks them.
    pub already_broken: Vec<(&'t MoveInvariant, GuardFailure)>,
}

impl TransitionTable {
    /// Compare the type's invariants before a move and after it. `before`
    /// and `after` are the status and the values the invariants read, before
    /// and after the move.
    pub fn invariants_across_move(
        &self,
        before: (&str, &EvalContext),
        after: (&str, &EvalContext),
    ) -> InvariantsAcrossMove<'_> {
        let mut result = InvariantsAcrossMove::default();
        for invariant in &self.invariants {
            if let Some(failure) =
                guard::check_detailed(&invariant.assert, before.0, before.1, &self.initial_values)
            {
                result.already_broken.push((invariant, failure));
                continue;
            }
            if invariant.repointed(before.1, after.1) {
                continue;
            }
            if let Some(failure) =
                guard::check_detailed(&invariant.assert, after.0, after.1, &self.initial_values)
            {
                result.broken = Some((invariant, failure));
                break;
            }
        }
        result
    }

    /// The first invariant that is false for a record in `status` with the
    /// values in `ctx`, with its failing part. Those that read another
    /// record's status are left out: the record alone cannot answer them.
    pub fn broken_invariant(
        &self,
        status: &str,
        ctx: &EvalContext,
    ) -> Option<(&MoveInvariant, GuardFailure)> {
        self.invariants
            .iter()
            .filter(|invariant| invariant.assert.cross_refs().is_empty())
            .find_map(|invariant| {
                guard::check_detailed(&invariant.assert, status, ctx, &self.initial_values)
                    .map(|failure| (invariant, failure))
            })
    }
}

#[cfg(test)]
#[path = "invariant_test.rs"]
mod tests;
