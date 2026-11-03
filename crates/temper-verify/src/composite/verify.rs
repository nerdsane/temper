//! Bounded joint-state verification entry point (ADR-0150).
//!
//! Ties the trigger graph, [`CompositeVerificationPlan`], and the joint
//! model ([`super::joint`]) together into an always-on cross-entity check:
//!
//! 1. [`seed_cover`] picks one seed per weakly-connected component of the
//!    entity trigger graph, so every entity is covered by exactly one plan
//!    and no entity is left unverified.
//! 2. [`verify_composite`] walks the joint model of the seed's component
//!    (one record of each type, nothing sliced) breadth first, in the memory
//!    its [`Room`](super::Room) gives it, as the joint checks do, and names
//!    every reaction a target refuses when it lands. The dispatches a move
//!    sends land, in any order, before the next move from outside. If the
//!    walk does not fit before the space is exhausted, the result is marked
//!    [`CompositeOutcome::Incomplete`], never a silent pass.
//! 3. [`verify_all`] runs every seed and aggregates.
//!
//! Determinism: weakly-connected components are computed over `BTreeSet`s
//! and seeds are the lexicographically smallest entity per component, so the
//! seed set and its order are stable across runs (DST requirement).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use temper_spec::automaton::{Automaton, TriggerGraph};

use super::joint::{Bound, DroppedReaction, dropped_reactions};
use super::{CompositePlanError, CompositeVerificationPlan};

/// Outcome of a bounded composite BFS run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompositeOutcome {
    /// BFS explored the whole reachable joint space and all properties held.
    Verified,
    /// At least one property (e.g. `no_dropped_reaction`) was violated.
    Violated,
    /// The walk did not fit in its memory before exhausting the space. The
    /// proof is partial: discovered violations are still real, but absence
    /// of a violation does NOT mean the property holds. Never treated as a
    /// pass.
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
    /// Each distinct dropped reaction found.
    pub dropped_reactions: Vec<DroppedReaction>,
    /// Why the walk did not see every joint state, when it did not.
    pub incomplete: Option<String>,
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

    // Collect the canonical root of each component.
    let mut roots: BTreeSet<String> = BTreeSet::new();
    let entities: Vec<String> = graph.entities.iter().cloned().collect();
    for entity in &entities {
        let root = find(&mut parent, entity);
        roots.insert(root);
    }
    roots.into_iter().collect()
}

/// Verify the composite rooted at `seed`, its walk taking memory from
/// `bound`'s room. See [`CompositeOutcome::Incomplete`].
pub fn verify_composite(
    automatons: &[&Automaton],
    seed: &str,
    bound: Bound<'_>,
) -> Result<CompositeVerifyResult, CompositePlanError> {
    let plan = CompositeVerificationPlan::new(automatons, seed)?;
    let scope: BTreeSet<String> = plan.models.into_keys().collect();
    let found = dropped_reactions(automatons, scope.clone(), bound);
    let outcome = if !found.drops.is_empty() {
        CompositeOutcome::Violated
    } else if found.stopped.is_none() {
        CompositeOutcome::Verified
    } else {
        CompositeOutcome::Incomplete
    };
    Ok(CompositeVerifyResult {
        seed: seed.to_string(),
        scope: scope.into_iter().collect(),
        outcome,
        states_explored: found.states,
        dropped_reactions: found.drops,
        incomplete: found.stopped,
    })
}

/// Run composite verification across the seed cover, returning one result
/// per seed. The aggregate is gating: any [`CompositeOutcome::Violated`]
/// fails; any [`CompositeOutcome::Incomplete`] is surfaced as a warning by
/// the caller (the proof is partial, not a pass). Each seed's walk takes
/// memory from `bound`'s room and gives it back when it ends.
pub fn verify_all(automatons: &[&Automaton], bound: Bound<'_>) -> Vec<CompositeVerifyResult> {
    let seeds = seed_cover(automatons);
    let mut results = Vec::with_capacity(seeds.len());
    for seed in seeds {
        match verify_composite(automatons, &seed, bound) {
            Ok(result) => results.push(result),
            Err(error) => {
                // A seed that cannot build a plan (e.g. a trigger to an
                // entity outside the supplied set) is reported as an
                // incomplete result so the caller never reads it as a pass.
                results.push(CompositeVerifyResult {
                    seed: seed.clone(),
                    scope: vec![seed],
                    outcome: CompositeOutcome::Incomplete,
                    states_explored: 0,
                    dropped_reactions: Vec::new(),
                    incomplete: Some(error.to_string()),
                });
            }
        }
    }
    results
}

#[cfg(test)]
#[path = "verify_test.rs"]
mod tests;
