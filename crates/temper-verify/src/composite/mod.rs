//! Composite verification for cross-entity trigger chains (ADR-0046 slice 8).
//!
//! Builds on [`temper_spec::automaton::TriggerGraph`] to compose multiple
//! entity [`TemperModel`](crate::TemperModel)s into a joint verification
//! unit. Given a seed entity and the tenant's parsed automatons, this
//! module:
//!
//! 1. Finds the seed's complete weakly-connected trigger component.
//! 2. Builds a [`TemperModel`] for each participating entity.
//! 3. Materialises a [`CompositeVerificationPlan`] describing what the
//!    joint state-machine verifier would check.
//! 4. (Slice 8d) Implements [`stateright::Model`] over the composition
//!    ([`CompositeTemperModel`]), so the verifier can BFS the joint
//!    state space with cross-entity cascades applied within each step.
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

mod cascade;
pub mod invariant_eval;
pub mod model;
pub mod verify;

pub use model::{CompositeAction, CompositeState, CompositeTemperModel, DroppedReaction};
pub use verify::{
    CompositeOutcome, CompositeVerifyResult, seed_cover, verify_all, verify_composite,
    verify_composite_with_budget,
};

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use temper_spec::automaton::{Automaton, TriggerEdge, TriggerGraph};

use crate::model::{TemperModel, build_model_from_automaton};

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
        }
    }
}

impl std::error::Error for CompositePlanError {}

/// Plan for verifying a joint state machine formed by a seed entity and
/// every other entity reachable from it via entity-kind triggers.
///
/// Holds per-entity [`TemperModel`]s (indexed by entity type name) plus
/// the trigger edges that link them. The future composite verifier (slice
/// 8c) consumes this directly — state becomes
/// `BTreeMap<EntityType, TemperModelState>`, actions become
/// `(EntityType, TemperModelAction)`, and next_state walks `edges` to
/// apply triggered transitions to target entities.
pub struct CompositeVerificationPlan {
    /// Seed entity this plan was rooted at.
    pub seed: String,
    /// One [`TemperModel`] per entity in the composition scope.
    /// Keyed by entity type name for deterministic iteration.
    pub models: BTreeMap<String, TemperModel>,
    /// Trigger edges within the composition scope. Edges to/from
    /// entities outside the scope are filtered out at build time.
    pub edges: Vec<TriggerEdge>,
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
            .field("has_cycle", &self.has_cycle)
            .finish()
    }
}

impl CompositeVerificationPlan {
    /// Build a [`CompositeVerificationPlan`] from a set of parsed
    /// automatons and a seed entity.
    ///
    /// Walks the trigger graph from the seed, collects every reachable
    /// entity, and builds a per-entity [`TemperModel`] for each. Returns
    /// errors for missing seeds, missing trigger targets, and per-entity
    /// model-build failures.
    pub fn new(automatons: &[&Automaton], seed: &str) -> Result<Self, CompositePlanError> {
        let graph = TriggerGraph::from_automatons(automatons);
        if !graph.entities.contains(seed) {
            return Err(CompositePlanError::SeedMissing(seed.to_string()));
        }

        // The seed is the canonical representative of a weak component. A
        // directed reachability walk is insufficient when the smallest member
        // has only incoming edges (Z -> A): include both endpoints of every
        // incident edge until the component reaches a fixed point.
        let mut scope = BTreeSet::from([seed.to_string()]);
        loop {
            let before = scope.len();
            for edges in graph.outgoing.values() {
                for edge in edges {
                    if scope.contains(&edge.from) || scope.contains(&edge.to) {
                        scope.insert(edge.from.clone());
                        scope.insert(edge.to.clone());
                    }
                }
            }
            if scope.len() == before {
                break;
            }
        }
        let has_cycle = graph.has_cycle_from(seed);

        // Index automatons by entity type name for O(log n) lookup.
        let by_name: BTreeMap<&str, &&Automaton> = automatons
            .iter()
            .map(|a| (a.automaton.name.as_str(), a))
            .collect();

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
            let aut = by_name
                .get(entity.as_str())
                .expect("scope member is in input set");
            let model = build_model_from_automaton(aut, DEFAULT_COMPOSITE_MAX_COUNTER);
            models.insert(entity.clone(), model);
        }

        Ok(Self {
            seed: seed.to_string(),
            models,
            edges,
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

#[cfg(test)]
mod tests {
    use super::*;
    use temper_spec::automaton::parse_automaton;

    fn order_ioa() -> &'static str {
        r#"
[automaton]
name = "Order"
states = ["Draft", "Submitted", "Confirmed"]
initial = "Draft"

[[action]]
name = "SubmitOrder"
from = ["Draft"]
to = "Submitted"

[[action]]
name = "ConfirmOrder"
from = ["Submitted"]
to = "Confirmed"

[[action.triggers]]
name = "confirm_triggers_auth"
kind = "entity"
principal = "payment-service"
target_entity = "Payment"
target_action = "AuthorizePayment"

[action.triggers.resolve_target]
type = "field"
field = "payment_id"
"#
    }

    fn payment_ioa() -> &'static str {
        r#"
[automaton]
name = "Payment"
states = ["Pending", "Authorized"]
initial = "Pending"

[[action]]
name = "AuthorizePayment"
from = ["Pending"]
to = "Authorized"
"#
    }

    fn wiki_ioa() -> &'static str {
        r#"
[automaton]
name = "Wiki"
states = ["Draft", "Published"]
initial = "Draft"

[[action]]
name = "Publish"
from = ["Draft"]
to = "Published"
"#
    }

    #[test]
    fn plan_from_two_entity_chain_collects_both() {
        let order = parse_automaton(order_ioa()).unwrap();
        let payment = parse_automaton(payment_ioa()).unwrap();
        let plan = CompositeVerificationPlan::new(&[&order, &payment], "Order")
            .expect("plan should build");
        assert_eq!(plan.seed, "Order");
        assert_eq!(plan.scope_size(), 2, "Order + Payment in scope");
        assert_eq!(plan.edge_count(), 1);
        assert!(!plan.has_cycle);
    }

    #[test]
    fn plan_excludes_unrelated_entities() {
        let order = parse_automaton(order_ioa()).unwrap();
        let payment = parse_automaton(payment_ioa()).unwrap();
        let wiki = parse_automaton(wiki_ioa()).unwrap();
        let plan = CompositeVerificationPlan::new(&[&order, &payment, &wiki], "Order")
            .expect("plan should build");
        assert_eq!(plan.scope_size(), 2, "Wiki excluded from Order scope");
        assert!(!plan.models.contains_key("Wiki"));
    }

    #[test]
    fn plan_from_isolated_entity_has_no_edges() {
        let wiki = parse_automaton(wiki_ioa()).unwrap();
        let plan = CompositeVerificationPlan::new(&[&wiki], "Wiki")
            .expect("plan should build for isolated entity");
        assert_eq!(plan.scope_size(), 1);
        assert_eq!(plan.edge_count(), 0);
    }

    #[test]
    fn missing_seed_errors() {
        let wiki = parse_automaton(wiki_ioa()).unwrap();
        let err = CompositeVerificationPlan::new(&[&wiki], "NotAnEntity")
            .expect_err("missing seed must error");
        assert!(matches!(err, CompositePlanError::SeedMissing(_)));
    }

    #[test]
    fn trigger_pointing_to_missing_entity_errors() {
        // Order references Payment but Payment is not supplied.
        let order = parse_automaton(order_ioa()).unwrap();
        let err = CompositeVerificationPlan::new(&[&order], "Order")
            .expect_err("missing target must error");
        assert!(matches!(
            err,
            CompositePlanError::UnknownTriggerTarget { .. }
        ));
    }

    #[test]
    fn summary_renders_edges_and_scope() {
        let order = parse_automaton(order_ioa()).unwrap();
        let payment = parse_automaton(payment_ioa()).unwrap();
        let plan = CompositeVerificationPlan::new(&[&order, &payment], "Order").unwrap();
        let summary = plan.summary();
        assert!(summary.contains("Order"));
        assert!(summary.contains("Payment"));
        assert!(summary.contains("ConfirmOrder"));
        assert!(summary.contains("AuthorizePayment"));
    }

    #[test]
    fn liveness_required_flag_propagates_to_plan() {
        let spec_with_liveness = r#"
[automaton]
name = "A"
states = ["X", "Y"]
initial = "X"

[[action]]
name = "Go"
from = ["X"]
to = "Y"

[[action.triggers]]
name = "must_fire"
kind = "entity"
liveness = "required"
target_entity = "B"
target_action = "Do"

[action.triggers.resolve_target]
type = "same_id"
"#;
        let spec_b = r#"
[automaton]
name = "B"
states = ["Idle", "Done"]
initial = "Idle"

[[action]]
name = "Do"
from = ["Idle"]
to = "Done"
"#;
        let a = parse_automaton(spec_with_liveness).unwrap();
        let b = parse_automaton(spec_b).unwrap();
        let plan = CompositeVerificationPlan::new(&[&a, &b], "A").unwrap();
        assert!(plan.requires_liveness());
    }
}
