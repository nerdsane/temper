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
//! 3. [`verify_all`] runs every seed and aggregates. Optional sidecar
//!    (`cross-invariants.toml`) unions `related()` pairs into the cover
//!    and adds the `related_field_constraints` property (ADR-0171).
//!
//! Determinism: weakly-connected components are computed over `BTreeSet`s
//! and seeds are the lexicographically smallest entity per component, so the
//! seed set and its order are stable across runs (DST requirement).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use stateright::{Checker, Model};
use temper_spec::automaton::{Automaton, TriggerGraph};
use temper_spec::cross_invariant::CrossInvariantSpec;

use super::model::{CompositeTemperModel, DroppedReaction};
use super::related_field::{RelatedFieldViolation, related_composition_pairs};
use super::{CompositePlanError, CompositeVerificationPlan};

/// Default joint-state BFS budget (target unique-state count). The checker
/// may slightly exceed it for performance, but stops near it; if the space
/// is larger, the run is reported [`CompositeOutcome::Incomplete`].
///
/// Sized to comfortably cover small app compositions (a handful of entities,
/// each with a few states + bounded counters) while bounding pathological
/// products. Tune via [`verify_composite_with_budget`] when needed.
pub const DEFAULT_COMPOSITE_STATE_BUDGET: usize = 250_000;

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
    /// invariant), for completeness. Includes hard related-field row names.
    pub other_violations: Vec<String>,
    /// Hard related-field counterexamples (one per distinct row name).
    pub related_field_violations: Vec<RelatedFieldViolation>,
}

impl CompositeVerifyResult {
    /// `true` only when the run fully explored the space and found no
    /// violation. An incomplete run is never "passed".
    pub fn passed(&self) -> bool {
        self.outcome == CompositeOutcome::Verified
    }
}

/// Seed cover: one seed per weakly-connected component of the trigger graph
/// union hard related-field `on` ↔ `related()` pairs (ADR-0171).
///
/// Every entity belongs to exactly one weakly-connected component, so seeding
/// each component's root reaches every entity (via the composite plan's
/// reachability closure). The root is the lexicographically smallest entity
/// in the component for deterministic output. Returns seeds in sorted order.
pub fn seed_cover(automatons: &[&Automaton], sidecar: Option<&CrossInvariantSpec>) -> Vec<String> {
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

    if let Some(spec) = sidecar {
        for (on_entity, target, _) in related_composition_pairs(spec) {
            if parent.contains_key(&on_entity) && parent.contains_key(&target) {
                union(&mut parent, &on_entity, &target);
            }
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
    sidecar: Option<&CrossInvariantSpec>,
) -> Result<CompositeVerifyResult, CompositePlanError> {
    verify_composite_with_budget(automatons, seed, DEFAULT_COMPOSITE_STATE_BUDGET, sidecar)
}

/// Verify the composite rooted at `seed`, bounding the BFS to roughly
/// `state_budget` unique states. See [`CompositeOutcome::Incomplete`].
pub fn verify_composite_with_budget(
    automatons: &[&Automaton],
    seed: &str,
    state_budget: usize,
    sidecar: Option<&CrossInvariantSpec>,
) -> Result<CompositeVerifyResult, CompositePlanError> {
    let plan = CompositeVerificationPlan::new_with_sidecar(automatons, seed, sidecar)?;
    let scope: Vec<String> = plan.models.keys().cloned().collect();
    let model = CompositeTemperModel::from_plan(plan);

    let checker = model
        .checker()
        .target_state_count(state_budget)
        .spawn_bfs()
        .join();

    let states_explored = checker.unique_state_count();
    // The run is genuinely complete only when the checker exhausted the space
    // WITHOUT the budget ever gating it. `is_done()` alone is unreliable here:
    // a worker that stops on the state-count budget still drops its job-broker
    // handle, which can flip `is_done()` true even though jobs remained. So we
    // additionally require that exploration stayed strictly under the budget —
    // if it reached the ceiling, the budget may have cut it short and we must
    // report INCOMPLETE rather than claim a pass.
    let property_complete = checker.is_done() && states_explored < state_budget;

    // Collect every property violation that is not expanded separately
    // (`no_dropped_reaction`, `related_field_constraints`).
    let mut other_violations = Vec::new();
    for (property_name, _path) in checker.discoveries() {
        if property_name != "no_dropped_reaction" && property_name != "related_field_constraints" {
            other_violations.push(property_name.to_string());
        }
    }

    // Enumerate EVERY distinct dropped reaction, not just the single
    // counterexample Stateright surfaces — authors want to see all of them in
    // one run. `enumerate_drops` walks the reachable joint space directly.
    let model = checker.model();
    let (dropped_reactions, drops_complete) = model.enumerate_drops(state_budget);
    let related_field_violations = model.enumerate_related_field_violations(state_budget);
    for violation in &related_field_violations {
        if !other_violations.iter().any(|n| n == &violation.name) {
            other_violations.push(violation.name.clone());
        }
    }

    let is_complete = property_complete && drops_complete;
    let has_violation = !dropped_reactions.is_empty()
        || !other_violations.is_empty()
        || !related_field_violations.is_empty();
    let outcome = if has_violation {
        CompositeOutcome::Violated
    } else if is_complete {
        CompositeOutcome::Verified
    } else {
        CompositeOutcome::Incomplete
    };

    Ok(CompositeVerifyResult {
        seed: seed.to_string(),
        scope,
        outcome,
        states_explored,
        dropped_reactions,
        other_violations,
        related_field_violations,
    })
}

/// Run composite verification across the seed cover, returning one result
/// per seed. The aggregate is gating: any [`CompositeOutcome::Violated`]
/// fails; any [`CompositeOutcome::Incomplete`] is surfaced as a warning by
/// the caller (the proof is partial, not a pass).
pub fn verify_all(
    automatons: &[&Automaton],
    sidecar: Option<&CrossInvariantSpec>,
) -> Vec<CompositeVerifyResult> {
    let seeds = seed_cover(automatons, sidecar);
    let mut results = Vec::with_capacity(seeds.len());
    for seed in seeds {
        match verify_composite(automatons, &seed, sidecar) {
            Ok(result) => results.push(result),
            Err(err) => {
                // A seed that cannot build a plan (e.g. a trigger or related()
                // target outside the supplied set) is reported as incomplete
                // so the caller never reads it as a pass.
                results.push(CompositeVerifyResult {
                    seed: seed.clone(),
                    scope: vec![seed],
                    outcome: CompositeOutcome::Incomplete,
                    states_explored: 0,
                    dropped_reactions: Vec::new(),
                    other_violations: vec![format!("plan build failed: {err}")],
                    related_field_violations: Vec::new(),
                });
            }
        }
    }
    results
}

#[cfg(test)]
#[path = "verify_test.rs"]
mod tests;
