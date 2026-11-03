//! Phase two by any order, for measuring one against [`plain`] offline.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::Automaton;
use temper_spec::predicate::{Expr, Truth, eval};

use super::super::super::JointOutcome;
use super::super::super::room::{Bound, Bytes, Progress};
use super::super::{JointModel, Sending};
use super::{Budget, Guide, Stats};
#[cfg(doc)]
use super::{STEPS, plain};

/// What phase two did on one entity's invariants, for measuring an order.
#[derive(Debug, Clone)]
pub struct GuidedSearch {
    /// States the breadth-first walk expanded: until it met a break, its
    /// room ran out, or it saw every state.
    pub bfs_states: usize,
    /// The breadth-first walk met a break itself; phase two did not run.
    pub bfs_found: bool,
    /// Expansions and distinct states phase two had made when it confirmed
    /// its first break (a trace that replays); `None` when it found none.
    pub found: Option<(usize, usize)>,
    /// Walks, expansions, distinct states and the deepest trace, in all.
    pub walks: usize,
    pub steps: usize,
    pub distinct: usize,
    pub deepest: usize,
    /// The verdicts, one per invariant, as an install words them.
    pub outcomes: Vec<String>,
}

/// Check `owner`'s invariants that read related records in `automatons` as
/// an install does (without Cedar, so every sender may send everything),
/// with the breadth-first walk in a room of `room` bytes and phase two
/// ordered by `order` within `steps` expansions. Installs use [`plain`] and
/// [`STEPS`]; this is how another order is measured against it offline:
/// place breaks past what the breadth-first walk reaches, and count the
/// expansions and states each order needs to find them.
pub fn guided_search(
    automatons: &[&Automaton],
    owner: &str,
    room: usize,
    steps: usize,
    order: &mut Guide<'_>,
) -> Result<GuidedSearch, String> {
    let by_name: BTreeMap<&str, &Automaton> = automatons
        .iter()
        .map(|a| (a.automaton.name.as_str(), *a))
        .collect();
    let automaton = by_name
        .get(owner)
        .ok_or_else(|| format!("no entity type {owner}"))?;
    let invariants: Vec<&Expr> = automaton
        .invariants
        .iter()
        .filter(|inv| !inv.assert.related_types().is_empty())
        .map(|inv| &inv.assert)
        .collect();
    let mut scope = BTreeSet::from([owner.to_string()]);
    for inv in &invariants {
        for (entity, _) in inv.related_types() {
            if !by_name.contains_key(entity.as_str()) {
                return Err(format!("{owner} reads {entity}, which is not declared"));
            }
            scope.insert(entity);
        }
    }
    let sending = Sending {
        policies: None,
        reserve: None,
        kernel_moves: true,
        relies_on: BTreeSet::new(),
    };
    let model = JointModel::new(&by_name, &scope, owner, &invariants, sending, true)
        .without_silent(&by_name);
    let (bytes, progress) = (Bytes::new(room), Progress::default());
    let bound = Bound {
        room: &bytes,
        progress: &progress,
    };
    let mut expanded = 0;
    let mut met = false;
    let explored = model.walk_with(bound, false, |_, state| {
        expanded += 1;
        if state.settled() {
            let env = model.env(owner, state);
            met = invariants.iter().any(|inv| eval(inv, &env) == Truth::False);
        }
        met
    });
    let outcomes: Vec<JointOutcome> = invariants
        .iter()
        .map(|_| JointOutcome::Incomplete {
            reason: explored.stopped.clone().unwrap_or_default(),
        })
        .collect();
    let budget = Budget {
        steps,
        confirm: steps / 8,
    };
    let (outcomes, stats) = if met {
        (outcomes, Stats::default())
    } else {
        model.guided_invariants_by(
            owner,
            &invariants,
            outcomes,
            explored,
            bound,
            (order, budget),
        )
    };
    Ok(GuidedSearch {
        bfs_states: expanded,
        bfs_found: met,
        found: stats.found,
        walks: stats.walks,
        steps: stats.steps,
        distinct: stats.distinct,
        deepest: stats.deepest,
        outcomes: outcomes
            .iter()
            .map(|outcome| match outcome {
                JointOutcome::Holds => "HOLDS".to_string(),
                JointOutcome::Broken { path } => format!("BROKEN after {} moves", path.len() - 1),
                JointOutcome::Vacuous { .. } => "CHECKS NOTHING".to_string(),
                JointOutcome::Incomplete { reason } => format!("NOT CHECKED: {reason}"),
            })
            .collect(),
    })
}
