//! Bounded joint-state verification entry point (ADR-0150).
//!
//! Ties the trigger graph, [`CompositeVerificationPlan`], and
//! [`CompositeTemperModel`] together into an always-on cross-entity check:
//!
//! 1. [`seed_cover`] picks one seed per weakly-connected component of the
//!    entity trigger graph, so every entity is covered by exactly one plan
//!    and no entity is left unverified.
//! 2. [`verify_composite`] builds the plan + model for a seed and runs a
//!    BFS bounded by a target state-count budget. If the budget is
//!    exhausted before the space is, the result is marked
//!    [`CompositeOutcome::Incomplete`] — never a silent pass.
//! 3. [`verify_all`] runs every seed and aggregates.
//!
//! Determinism: weakly-connected components are computed over `BTreeSet`s
//! and seeds are the lexicographically smallest entity per component, so the
//! seed set and its order are stable across runs (DST requirement).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use temper_spec::automaton::{Automaton, TriggerGraph};

use super::model::{CompositeTemperModel, DroppedReaction};
use super::{CompositePlanError, CompositeVerificationPlan};

/// Default joint-state BFS budget (unique joint states after join-vector
/// projection). If the unique space is larger, the run is reported
/// [`CompositeOutcome::Incomplete`]. Tune via [`verify_composite_with_budget`].
pub const DEFAULT_COMPOSITE_STATE_BUDGET: usize = 1_000_000;

/// Outcome of a bounded composite BFS run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeOutcome {
    /// BFS explored the whole reachable joint space and all properties held.
    Verified,
    /// At least one property (e.g. `no_dropped_reaction`) was violated.
    Violated,
    /// BFS hit the state budget before exhausting the space. The proof is
    /// partial: discovered violations are still real, but absence of a
    /// violation does NOT mean the property holds. Never treated as a pass.
    Incomplete,
}

impl fmt::Display for CompositeOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompositeOutcome::Verified => write!(f, "VERIFIED"),
            CompositeOutcome::Violated => write!(f, "VIOLATED"),
            CompositeOutcome::Incomplete => write!(f, "INCOMPLETE"),
        }
    }
}

/// Result of verifying one composite plan (one seed).
#[derive(Debug, Clone)]
pub struct CompositeVerifyResult {
    /// The seed entity this plan was rooted at.
    pub seed: String,
    /// Entity types composed for this seed (the reachability scope).
    pub scope: Vec<String>,
    /// Overall outcome.
    pub outcome: CompositeOutcome,
    /// Unique joint states explored.
    pub states_explored: usize,
    /// Dropped-reaction counterexamples discovered (one per distinct drop
    /// witnessed at the end of a counterexample path).
    pub dropped_reactions: Vec<DroppedReaction>,
    /// Names of any other violated properties (e.g. a joint local
    /// invariant), for completeness.
    pub other_violations: Vec<String>,
}

impl CompositeVerifyResult {
    /// `true` only when the run fully explored the space and found no
    /// violation. An incomplete run is never "passed".
    pub fn passed(&self) -> bool {
        self.outcome == CompositeOutcome::Verified
    }
}

/// Seed cover: one seed per weakly-connected component of the trigger graph.
///
/// Every entity belongs to exactly one weakly-connected component, so seeding
/// each component's root reaches every entity (via the composite plan's
/// reachability closure). The root is the lexicographically smallest entity
/// in the component for deterministic output. Returns seeds in sorted order.
pub fn seed_cover(automatons: &[&Automaton]) -> Vec<String> {
    let graph = TriggerGraph::from_automatons(automatons);

    // Union-find over the UNDIRECTED projection of the trigger graph: two
    // entities are in the same component if a reaction edge connects them in
    // either direction.
    let mut parent: BTreeMap<String, String> = graph
        .entities
        .iter()
        .map(|e| (e.clone(), e.clone()))
        .collect();

    fn find(parent: &mut BTreeMap<String, String>, node: &str) -> String {
        let mut cur = node.to_string();
        while parent[&cur] != cur {
            let grand = parent[&parent[&cur]].clone();
            parent.insert(cur.clone(), grand.clone());
            cur = grand;
        }
        cur
    }

    fn union(parent: &mut BTreeMap<String, String>, a: &str, b: &str) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            // Attach the lexicographically larger root under the smaller, so
            // each component's representative is its smallest member.
            let (small, large) = if ra < rb { (ra, rb) } else { (rb, ra) };
            parent.insert(large, small);
        }
    }

    for edges in graph.outgoing.values() {
        for edge in edges {
            if parent.contains_key(&edge.from) && parent.contains_key(&edge.to) {
                union(&mut parent, &edge.from, &edge.to);
            }
        }
    }
    // A cross-entity *guard* is a joint coupling even when no trigger fires.
    // Without this, CuratorAgent × ReviewAgent compose (submit creates a
    // review) while DesignLanguage stays out of scope and the "must be
    // UnderReview" guard stays a free boolean.
    for (from, to) in temper_spec::automaton::guard_couplings(automatons) {
        if parent.contains_key(&from) && parent.contains_key(&to) {
            union(&mut parent, &from, &to);
        }
    }

    // Collect the canonical root of each component.
    let mut roots: BTreeSet<String> = BTreeSet::new();
    let entities: Vec<String> = graph.entities.iter().cloned().collect();
    for entity in &entities {
        let root = find(&mut parent, entity);
        roots.insert(root);
    }
    roots.into_iter().collect()
}

/// Verify the composite rooted at `seed` with the default state budget.
pub fn verify_composite(
    automatons: &[&Automaton],
    seed: &str,
) -> Result<CompositeVerifyResult, CompositePlanError> {
    verify_composite_with_budget(automatons, seed, DEFAULT_COMPOSITE_STATE_BUDGET)
}

/// Verify the composite rooted at `seed`, bounding the unique-state BFS
/// to `state_budget`. See [`CompositeOutcome::Incomplete`].
pub fn verify_composite_with_budget(
    automatons: &[&Automaton],
    seed: &str,
    state_budget: usize,
) -> Result<CompositeVerifyResult, CompositePlanError> {
    let plan = CompositeVerificationPlan::new(automatons, seed)?;
    let scope: Vec<String> = plan.models.keys().cloned().collect();
    let model = CompositeTemperModel::from_plan(plan);

    // Unique-state BFS is the source of truth. Stateright's
    // `target_state_count` counts generated edges, which over-counts
    // machines with many status self-loops and reports Incomplete on a
    // join that has already been fully seen.
    let walk = model.explore_unique(state_budget);

    let mut other_violations = Vec::new();
    if walk.invariant_failed {
        other_violations.push("joint_local_invariants".to_string());
    }

    let has_violation = !walk.dropped_reactions.is_empty() || !other_violations.is_empty();
    let outcome = if has_violation {
        CompositeOutcome::Violated
    } else if walk.complete {
        CompositeOutcome::Verified
    } else {
        CompositeOutcome::Incomplete
    };

    Ok(CompositeVerifyResult {
        seed: seed.to_string(),
        scope,
        outcome,
        states_explored: walk.unique_states,
        dropped_reactions: walk.dropped_reactions,
        other_violations,
    })
}

/// Run composite verification across the seed cover, returning one result
/// per seed. The aggregate is gating: any [`CompositeOutcome::Violated`]
/// fails; any [`CompositeOutcome::Incomplete`] is surfaced as a warning by
/// the caller (the proof is partial, not a pass).
pub fn verify_all(automatons: &[&Automaton]) -> Vec<CompositeVerifyResult> {
    verify_all_with_budget(automatons, DEFAULT_COMPOSITE_STATE_BUDGET)
}

/// Same as [`verify_all`], with an explicit joint-state BFS budget.
pub fn verify_all_with_budget(
    automatons: &[&Automaton],
    state_budget: usize,
) -> Vec<CompositeVerifyResult> {
    let seeds = seed_cover(automatons);
    let mut results = Vec::with_capacity(seeds.len());
    for seed in seeds {
        match verify_composite_with_budget(automatons, &seed, state_budget) {
            Ok(result) => results.push(result),
            Err(_) => {
                // A seed that cannot build a plan (e.g. a trigger to an
                // entity outside the supplied set) is reported as an
                // incomplete result so the caller never reads it as a pass.
                results.push(CompositeVerifyResult {
                    seed: seed.clone(),
                    scope: vec![seed],
                    outcome: CompositeOutcome::Incomplete,
                    states_explored: 0,
                    dropped_reactions: Vec::new(),
                    other_violations: vec![
                        "plan build failed (unknown trigger target?)".to_string(),
                    ],
                });
            }
        }
    }
    results
}

#[cfg(test)]
#[path = "verify_test.rs"]
mod tests;
