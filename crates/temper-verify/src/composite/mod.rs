//! Composite verification for entity-kind trigger chains (JCS, ADR-0150)
//! plus hard related-field sidecar rows (ADR-0171).
//!
//! Builds on [`temper_spec::automaton::TriggerGraph`] to compose multiple
//! entity [`TemperModel`](crate::TemperModel)s into a joint verification
//! unit. JCS checks:
//!
//! - `joint_local_invariants` — each Automaton's `[[invariant]]` after reactions
//! - `no_dropped_reaction` — entity-kind `[[action.triggers]]` must be enabled
//!   on the target
//! - `related_field_constraints` — a hard sidecar row fails when a matching
//!   action is **enabled** while `related(...).field` does not hold (never
//!   synthesized as extra `actions()` guards)
//!
//! Eventual-kind sidecar rows stay runtime-only.
//!
//! Given a seed entity and the tenant's parsed automatons, this module:
//!
//! 1. Walks the trigger graph from the seed to determine the participating
//!    entity set (reachability closure).
//! 2. Builds a [`TemperModel`] for each participating entity.
//! 3. Materialises a [`CompositeVerificationPlan`] describing what the
//!    joint state-machine verifier would check.
//! 4. Implements [`stateright::Model`] over the composition
//!    ([`CompositeTemperModel`]), so the verifier can BFS the joint
//!    state space with reaction cascades applied within each step.
//!
//! Example:
//!
//! ```ignore
//! use temper_verify::composite::{CompositeVerificationPlan, CompositeTemperModel};
//! use temper_spec::automaton::parse_automaton;
//!
//! let order = parse_automaton(order_ioa)?;
//! let payment = parse_automaton(payment_ioa)?;
//! let plan = CompositeVerificationPlan::new(&[&order, &payment], "Order")?;
//! let model = CompositeTemperModel::from_plan(plan);
//! // Model is Stateright-checkable from here.
//! ```

pub mod invariant_eval;
pub mod model;
pub mod related_field;
pub mod verify;

pub use model::{CompositeAction, CompositeState, CompositeTemperModel, DroppedReaction};
pub use related_field::{RelatedFieldFailReason, RelatedFieldRule, RelatedFieldViolation};
pub use verify::{
    CompositeOutcome, CompositeVerifyResult, seed_cover, verify_all, verify_composite,
    verify_composite_with_budget,
};

use std::collections::BTreeMap;
use std::fmt;

use temper_spec::automaton::{Automaton, TriggerEdge, TriggerGraph};
use temper_spec::cross_invariant::CrossInvariantSpec;

use crate::model::{TemperModel, build_model_from_automaton};

use related_field::{compile_hard_related_field_rules, related_composition_pairs};

/// Default per-entity counter ceiling used when building composite
/// [`TemperModel`]s. Matches the single-entity cascade default.
const DEFAULT_COMPOSITE_MAX_COUNTER: usize = 3;

/// Error produced while building a composite verification plan.
#[derive(Debug)]
pub enum CompositePlanError {
    /// The seed entity is not present in the supplied automaton set.
    SeedMissing(String),
    /// A trigger references a target entity type not present in the
    /// supplied automaton set.
    UnknownTriggerTarget {
        /// Source entity type.
        source: String,
        /// Trigger name.
        trigger: String,
        /// Missing target entity type.
        target: String,
    },
    /// A hard related-field row names a target entity type not present
    /// in the supplied automaton set. Not a silent pass (ADR-0171).
    UnknownRelatedTarget {
        /// Entity named by the row's `on` selector.
        source: String,
        /// Sidecar row name.
        constraint: String,
        /// Missing `related()` target entity type.
        target: String,
    },
}

impl fmt::Display for CompositePlanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SeedMissing(name) => {
                write!(f, "seed entity '{name}' not found in automaton set")
            }
            Self::UnknownTriggerTarget {
                source,
                trigger,
                target,
            } => write!(
                f,
                "trigger '{trigger}' on '{source}' references unknown target entity '{target}'"
            ),
            Self::UnknownRelatedTarget {
                source,
                constraint,
                target,
            } => write!(
                f,
                "related-field constraint '{constraint}' on '{source}' references unknown target entity '{target}'"
            ),
        }
    }
}

impl std::error::Error for CompositePlanError {}

/// Plan for verifying a joint state machine formed by a seed entity and
/// every other entity reachable from it via entity-kind triggers or hard
/// related-field sidecar pairs (ADR-0171).
///
/// Holds per-entity [`TemperModel`]s (indexed by entity type name) plus
/// the trigger edges that link them. The composite verifier consumes this
/// directly — state becomes `BTreeMap<EntityType, TemperModelState>`,
/// actions become `(EntityType, TemperModelAction)`, and next_state walks
/// `edges` to apply triggered transitions to target entities.
pub struct CompositeVerificationPlan {
    /// Seed entity this plan was rooted at.
    pub seed: String,
    /// One [`TemperModel`] per entity in the composition scope.
    /// Keyed by entity type name for deterministic iteration.
    pub models: BTreeMap<String, TemperModel>,
    /// Trigger edges within the composition scope. Edges to/from
    /// entities outside the scope are filtered out at build time.
    pub edges: Vec<TriggerEdge>,
    /// Hard related-field rules whose `on` entity is in this scope.
    pub related_field_rules: Vec<RelatedFieldRule>,
    /// Whether the trigger graph contains a cycle reachable from the seed.
    /// Not an error (cycles are legal; cascade depth bounds them at
    /// runtime and verification) but surfaced for reporting.
    pub has_cycle: bool,
}

impl fmt::Debug for CompositeVerificationPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompositeVerificationPlan")
            .field("seed", &self.seed)
            .field("models", &self.models.keys().collect::<Vec<_>>())
            .field("edges", &self.edges.len())
            .field("related_field_rules", &self.related_field_rules.len())
            .field("has_cycle", &self.has_cycle)
            .finish()
    }
}

impl CompositeVerificationPlan {
    /// Build a [`CompositeVerificationPlan`] from a set of parsed
    /// automatons and a seed entity (no sidecar).
    ///
    /// Walks the trigger graph from the seed, collects every reachable
    /// entity, and builds a per-entity [`TemperModel`] for each. Returns
    /// errors for missing seeds, missing trigger targets, and per-entity
    /// model-build failures.
    pub fn new(automatons: &[&Automaton], seed: &str) -> Result<Self, CompositePlanError> {
        Self::new_with_sidecar(automatons, seed, None)
    }

    /// Build a plan, unioning hard related-field `on` ↔ `related()` pairs
    /// into reachability so the target entity shares this seed (ADR-0171).
    pub fn new_with_sidecar(
        automatons: &[&Automaton],
        seed: &str,
        sidecar: Option<&CrossInvariantSpec>,
    ) -> Result<Self, CompositePlanError> {
        let graph = TriggerGraph::from_automatons(automatons);
        if !graph.entities.contains(seed) {
            return Err(CompositePlanError::SeedMissing(seed.to_string()));
        }

        let related_pairs = sidecar.map(related_composition_pairs).unwrap_or_default();
        let scope = composition_scope(&graph, seed, &related_pairs);
        let has_cycle = graph.has_cycle_from(seed);

        // Index automatons by entity type name for O(log n) lookup.
        let by_name: BTreeMap<&str, &&Automaton> = automatons
            .iter()
            .map(|a| (a.automaton.name.as_str(), a))
            .collect();

        for (on_entity, target, constraint) in &related_pairs {
            if scope.contains(on_entity) && !by_name.contains_key(target.as_str()) {
                return Err(CompositePlanError::UnknownRelatedTarget {
                    source: on_entity.clone(),
                    constraint: constraint.clone(),
                    target: target.clone(),
                });
            }
            if scope.contains(target) && !by_name.contains_key(on_entity.as_str()) {
                return Err(CompositePlanError::UnknownRelatedTarget {
                    source: on_entity.clone(),
                    constraint: constraint.clone(),
                    target: on_entity.clone(),
                });
            }
        }

        // Validate every edge inside scope points at an in-scope target
        // (they must, since reachability is transitive, but this also
        // catches mis-shaped graphs where an entity wasn't supplied).
        let mut edges: Vec<TriggerEdge> = Vec::new();
        for entity in &scope {
            if let Some(outgoing) = graph.outgoing.get(entity) {
                for edge in outgoing {
                    if !by_name.contains_key(edge.to.as_str()) {
                        return Err(CompositePlanError::UnknownTriggerTarget {
                            source: edge.from.clone(),
                            trigger: edge.trigger_name.clone(),
                            target: edge.to.clone(),
                        });
                    }
                    if scope.contains(&edge.to) {
                        edges.push(edge.clone());
                    }
                }
            }
        }

        // Build a TemperModel for each participating entity using the
        // direct-from-Automaton builder (no TOML round-trip).
        let mut models: BTreeMap<String, TemperModel> = BTreeMap::new();
        for entity in &scope {
            let aut = by_name.get(entity.as_str()).ok_or_else(|| {
                CompositePlanError::UnknownRelatedTarget {
                    source: seed.to_string(),
                    constraint: "related_field".to_string(),
                    target: entity.clone(),
                }
            })?;
            let model = build_model_from_automaton(aut, DEFAULT_COMPOSITE_MAX_COUNTER);
            models.insert(entity.clone(), model);
        }

        let related_field_rules = sidecar
            .map(compile_hard_related_field_rules)
            .unwrap_or_default()
            .into_iter()
            .filter(|rule| scope.contains(&rule.on_entity))
            .collect();

        Ok(Self {
            seed: seed.to_string(),
            models,
            edges,
            related_field_rules,
            has_cycle,
        })
    }

    /// Number of entities in the composition scope.
    pub fn scope_size(&self) -> usize {
        self.models.len()
    }

    /// Number of entity-kind trigger edges within the composition scope.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    /// Estimated joint state space size (product of per-entity state
    /// counts). Not the real reachable state count — that requires BFS —
    /// but an upper bound for budgeting. A plan whose `state_space_bound`
    /// exceeds, say, 1M should prompt the developer to factor the spec
    /// (fewer participating entities, or narrower triggers) before full
    /// model checking.
    pub fn state_space_bound(&self) -> usize {
        self.models.values().map(per_entity_state_bound).product()
    }

    /// Returns `true` if any edge in scope requests liveness verification.
    /// Signals to the composite verifier to emit `Property::eventually`
    /// for the matching target action.
    pub fn requires_liveness(&self) -> bool {
        self.edges.iter().any(|e| e.liveness_required)
    }

    /// Human-readable summary for CI output / dashboards.
    pub fn summary(&self) -> String {
        let entities: Vec<&str> = self.models.keys().map(String::as_str).collect();
        let edges_summary: Vec<String> = self
            .edges
            .iter()
            .map(|e| {
                let state = e
                    .to_state
                    .as_deref()
                    .map(|s| format!(" @{s}"))
                    .unwrap_or_default();
                let liveness = if e.liveness_required {
                    " (required)"
                } else {
                    ""
                };
                format!(
                    "{}.{}{} → {}.{}{}",
                    e.from, e.source_action, state, e.to, e.target_action, liveness
                )
            })
            .collect();
        format!(
            "CompositeVerificationPlan(seed={}, scope={:?}, edges=[{}], cycle={}, state_bound={})",
            self.seed,
            entities,
            edges_summary.join(", "),
            self.has_cycle,
            self.state_space_bound()
        )
    }
}

/// Reaction-space size hint used by [`CompositeVerificationPlan::state_space_bound`].
/// Conservative floor — the real bound would enumerate each entity's
/// reachable status set. For now, we approximate as 1 per entity; a later
/// refinement computes the status-set cardinality from the IOA.
fn per_entity_state_bound(_model: &TemperModel) -> usize {
    1
}

/// Directed trigger reachability from `seed`, plus undirected hard
/// related-field pairs (ADR-0171). Trigger edges stay directed so today's
/// trigger-only plans do not change; related() pairs pull the target in.
fn composition_scope(
    graph: &TriggerGraph,
    seed: &str,
    related_pairs: &[(String, String, String)],
) -> std::collections::BTreeSet<String> {
    use std::collections::{BTreeSet, VecDeque};

    let mut related_adj: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (on_entity, target, _) in related_pairs {
        related_adj
            .entry(on_entity.clone())
            .or_default()
            .insert(target.clone());
        related_adj
            .entry(target.clone())
            .or_default()
            .insert(on_entity.clone());
    }

    let mut visited: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();
    if !graph.entities.contains(seed) && !related_adj.contains_key(seed) {
        return visited;
    }
    queue.push_back(seed.to_string());
    while let Some(entity) = queue.pop_front() {
        if !visited.insert(entity.clone()) {
            continue;
        }
        if let Some(edges) = graph.outgoing.get(&entity) {
            for edge in edges {
                if !visited.contains(&edge.to) {
                    queue.push_back(edge.to.clone());
                }
            }
        }
        if let Some(neighbors) = related_adj.get(&entity) {
            for neighbor in neighbors {
                if !visited.contains(neighbor) {
                    queue.push_back(neighbor.clone());
                }
            }
        }
    }
    visited
}

#[cfg(test)]
#[path = "plan_test.rs"]
mod tests;
