//! Joint-state BFS verifier for trigger chains (ADR-0046 slice 8d).
//!
//! Implements [`stateright::Model`] over a composition of per-entity
//! [`TemperModel`]s, with trigger-induced cascades applied within a
//! single `next_state` call so the joint state space reflects
//! fire-and-forget runtime semantics.
//!
//! State: `BTreeMap<EntityType, TemperModelState>` — deterministic
//! iteration order for DST compliance.
//!
//! Action: tagged by source entity type plus the underlying
//! [`TemperModelAction`]. Only `actions()` advertises entities'
//! externally-enabled actions. Triggered cascades happen invisibly
//! inside `next_state` (bounded by `MAX_TRIGGER_DEPTH`).
//!
//! Invariants: each participating entity's local invariants are lifted
//! into the joint model via `Property::always` closures that project
//! the joint state down to the entity's slice before evaluating.

use std::collections::BTreeMap;
use std::fmt;

use stateright::{Model, Property};
use temper_spec::automaton::TriggerEdge;

use crate::model::{TemperModel, TemperModelAction, TemperModelState};

use super::CompositeVerificationPlan;

/// Maximum cascade depth per `next_state` call. Matches the runtime
/// `MAX_REACTION_DEPTH` (temper-server) so the verifier bounds exactly
/// what production also bounds.
const MAX_TRIGGER_DEPTH: u32 = 8;

/// Joint state over a composition of entities.
///
/// `BTreeMap` rather than `HashMap` for deterministic iteration order
/// (DST requirement — Stateright's BFS must produce identical state
/// enumerations across seeded runs).
#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompositeState {
    /// Per-entity state keyed by entity type name.
    pub entities: BTreeMap<String, TemperModelState>,
}

impl fmt::Display for CompositeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self
            .entities
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        write!(f, "[{}]", parts.join(" | "))
    }
}

/// Tagged action — identifies which entity is transitioning.
///
/// Only *externally enabled* actions appear in this enum. Triggered
/// cascade actions are absorbed inside `next_state` and don't show up
/// as separate Stateright actions.
#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompositeAction {
    /// Which entity the external caller is acting on.
    pub entity: String,
    /// The underlying action on that entity's IOA.
    pub action: TemperModelAction,
}

impl fmt::Display for CompositeAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.entity, self.action)
    }
}

/// Joint-state model: composes per-entity [`TemperModel`]s into a
/// Stateright-checkable product automaton with cross-entity trigger
/// cascades.
#[derive(Clone)]
pub struct CompositeTemperModel {
    /// Per-entity models keyed by entity type name.
    models: BTreeMap<String, TemperModel>,
    /// All entity-kind trigger edges within the composition scope.
    edges: Vec<TriggerEdge>,
    /// Entity types in the composition (cached for iteration).
    entity_types: Vec<String>,
}

impl CompositeTemperModel {
    /// Build a [`CompositeTemperModel`] from a [`CompositeVerificationPlan`].
    ///
    /// Consumes the plan — the underlying [`TemperModel`]s move into the
    /// joint model, and edges are cloned so the structure is self-contained.
    pub fn from_plan(plan: CompositeVerificationPlan) -> Self {
        let entity_types: Vec<String> = plan.models.keys().cloned().collect();
        Self {
            models: plan.models,
            edges: plan.edges,
            entity_types,
        }
    }

    /// Apply trigger cascades starting from a post-commit source action.
    ///
    /// Bounded by [`MAX_TRIGGER_DEPTH`]. Cycles are allowed (the runtime
    /// permits them) but depth caps prevent infinite exploration. Each
    /// triggered transition applies the target entity's matching action
    /// by name if enabled in the target's current state.
    fn cascade(
        &self,
        state: &mut CompositeState,
        source_entity: &str,
        source_action: &str,
        source_to_state: &str,
        depth: u32,
    ) {
        if depth >= MAX_TRIGGER_DEPTH {
            return;
        }
        let edges: Vec<&TriggerEdge> = self
            .edges
            .iter()
            .filter(|e| e.from == source_entity && e.source_action == source_action)
            .collect();
        for edge in edges {
            // to_state filter
            if let Some(expected) = &edge.to_state
                && expected != source_to_state
            {
                continue;
            }
            // Find target entity + model
            let Some(target_model) = self.models.get(&edge.to) else {
                continue;
            };
            let Some(target_state) = state.entities.get(&edge.to) else {
                continue;
            };

            // Is the target action enabled on the target's current state?
            let mut enabled = Vec::new();
            target_model.actions(target_state, &mut enabled);
            let Some(target_action) = enabled
                .iter()
                .find(|a| a.name == edge.target_action)
                .cloned()
            else {
                // Target action not enabled — fire-and-forget, skip silently
                // (matches runtime: guard-failed triggers don't produce a
                // dispatch record).
                continue;
            };

            // Apply target action
            let Some(new_target_state) =
                target_model.next_state(target_state, target_action.clone())
            else {
                continue;
            };

            let new_target_status = new_target_state.status.clone();
            state.entities.insert(edge.to.clone(), new_target_state);

            // Recurse: the target action itself may trigger further cascades.
            self.cascade(
                state,
                &edge.to,
                &target_action.name,
                &new_target_status,
                depth + 1,
            );
        }
    }
}

impl fmt::Debug for CompositeTemperModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompositeTemperModel")
            .field("entity_types", &self.entity_types)
            .field("edges", &self.edges.len())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::composite::CompositeVerificationPlan;
    use stateright::Checker;
    use temper_spec::automaton::parse_automaton;

    fn order_ioa() -> &'static str {
        r#"
[automaton]
name = "Order"
states = ["Draft", "Confirmed"]
initial = "Draft"
allow_indefinite_states = ["Draft", "Confirmed"]

[[action]]
name = "ConfirmOrder"
from = ["Draft"]
to = "Confirmed"

[[action.triggers]]
name = "auth_payment"
kind = "entity"
target_entity = "Payment"
target_action = "AuthorizePayment"

[action.triggers.resolve_target]
type = "same_id"
"#
    }

    fn payment_ioa() -> &'static str {
        r#"
[automaton]
name = "Payment"
states = ["Pending", "Authorized"]
initial = "Pending"
allow_indefinite_states = ["Pending", "Authorized"]

[[action]]
name = "AuthorizePayment"
from = ["Pending"]
to = "Authorized"
"#
    }

    #[test]
    fn bfs_explores_joint_state_space_with_cascade() {
        let order = parse_automaton(order_ioa()).unwrap();
        let payment = parse_automaton(payment_ioa()).unwrap();
        let plan = CompositeVerificationPlan::new(&[&order, &payment], "Order").unwrap();
        let model = CompositeTemperModel::from_plan(plan);

        let init_states = model.init_states();
        assert_eq!(init_states.len(), 1);
        let init = &init_states[0];
        assert_eq!(init.entities.get("Order").unwrap().status, "Draft");
        assert_eq!(init.entities.get("Payment").unwrap().status, "Pending");

        // Fire ConfirmOrder — cascade should advance Payment to Authorized.
        let mut actions = Vec::new();
        model.actions(init, &mut actions);
        let confirm = actions
            .iter()
            .find(|a| a.entity == "Order" && a.action.name == "ConfirmOrder")
            .expect("Order.ConfirmOrder enabled from Draft");
        let after = model.next_state(init, confirm.clone()).unwrap();
        assert_eq!(after.entities.get("Order").unwrap().status, "Confirmed");
        assert_eq!(
            after.entities.get("Payment").unwrap().status,
            "Authorized",
            "cascade should have triggered Payment.AuthorizePayment"
        );
    }

    #[test]
    fn bfs_checker_proves_joint_invariant_holds() {
        // Run Stateright BFS over the composite model — ensures no
        // joint state violates the local invariants.
        let order = parse_automaton(order_ioa()).unwrap();
        let payment = parse_automaton(payment_ioa()).unwrap();
        let plan = CompositeVerificationPlan::new(&[&order, &payment], "Order").unwrap();
        let model = CompositeTemperModel::from_plan(plan);
        let checker = model.checker().spawn_bfs().join();
        // "always" properties emit no discoveries when they hold.
        let discoveries = checker.discoveries();
        assert!(
            discoveries.is_empty(),
            "unexpected discoveries: {discoveries:?}"
        );
        // Ensure BFS actually visited multiple states.
        assert!(checker.unique_state_count() >= 2);
    }

    #[test]
    fn self_loop_cascade_bounded_by_max_depth() {
        // Self-triggering entity (Assign → Start on same entity) should
        // not blow up the state space; cascade bound stops it.
        let spec = r#"
[automaton]
name = "Agent"
states = ["Idle", "Assigned", "Working"]
initial = "Idle"
allow_indefinite_states = ["Idle", "Assigned", "Working"]

[[action]]
name = "Assign"
from = ["Idle"]
to = "Assigned"

[[action.triggers]]
name = "auto_start"
kind = "entity"
to_state = "Assigned"
target_entity = "Agent"
target_action = "Start"

[action.triggers.resolve_target]
type = "same_id"

[[action]]
name = "Start"
from = ["Assigned"]
to = "Working"
"#;
        let agent = parse_automaton(spec).unwrap();
        let plan = CompositeVerificationPlan::new(&[&agent], "Agent").unwrap();
        let model = CompositeTemperModel::from_plan(plan);
        let init = &model.init_states()[0];
        let mut actions = Vec::new();
        model.actions(init, &mut actions);
        let assign = actions
            .iter()
            .find(|a| a.action.name == "Assign")
            .cloned()
            .unwrap();
        let after = model.next_state(init, assign).unwrap();
        // The inline cascade fires Start automatically once — Agent lands in Working.
        assert_eq!(after.entities.get("Agent").unwrap().status, "Working");
    }
}

impl Model for CompositeTemperModel {
    type State = CompositeState;
    type Action = CompositeAction;

    fn init_states(&self) -> Vec<Self::State> {
        // Each entity starts in its single initial state. Cross-product is
        // therefore a single joint state.
        let mut entities = BTreeMap::new();
        for (entity_type, model) in &self.models {
            let init = model
                .init_states()
                .into_iter()
                .next()
                .expect("TemperModel always produces at least one init state");
            entities.insert(entity_type.clone(), init);
        }
        vec![CompositeState { entities }]
    }

    fn actions(&self, state: &Self::State, out: &mut Vec<Self::Action>) {
        // For each entity, yield its externally-enabled actions.
        // Triggered cascades are not separate actions (they fire inside
        // next_state), so we don't emit them here.
        for entity_type in &self.entity_types {
            let Some(entity_model) = self.models.get(entity_type) else {
                continue;
            };
            let Some(entity_state) = state.entities.get(entity_type) else {
                continue;
            };
            let mut per_entity = Vec::new();
            entity_model.actions(entity_state, &mut per_entity);
            for a in per_entity {
                out.push(CompositeAction {
                    entity: entity_type.clone(),
                    action: a,
                });
            }
        }
    }

    fn next_state(&self, state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let source_model = self.models.get(&action.entity)?;
        let source_state = state.entities.get(&action.entity)?;

        // Apply the primary (external) action to the actor entity.
        let new_source_state = source_model.next_state(source_state, action.action.clone())?;
        let new_source_status = new_source_state.status.clone();

        // Commit to joint state.
        let mut next = state.clone();
        next.entities
            .insert(action.entity.clone(), new_source_state);

        // Fire-and-forget cascade: apply triggered transitions
        // transitively, bounded by MAX_TRIGGER_DEPTH.
        self.cascade(
            &mut next,
            &action.entity,
            &action.action.name,
            &new_source_status,
            0,
        );

        Some(next)
    }

    fn properties(&self) -> Vec<Property<Self>> {
        // For slice 8d (v1) we use a single always-property that projects
        // the joint state down to each entity and checks the entity's
        // local invariants via a closure. Future refinement: emit per-
        // invariant properties so counterexamples identify which
        // invariant and which entity failed.
        //
        // We can't easily reuse TemperModel's `Property::always` fn pointers
        // (they take &TemperModel, not &CompositeTemperModel), so the
        // verifier runs per-entity BFS checks once and then runs a
        // composite always-property that evaluates user invariants
        // against each entity's slice using the local invariant
        // evaluator.
        //
        // Cross-entity invariants translation is a later slice; the
        // per-entity projection below is already more than snapshot
        // checking — Stateright's BFS visits every reachable joint
        // state.
        vec![Property::<Self>::always(
            "joint_local_invariants",
            |m, s| {
                for (entity_type, entity_state) in &s.entities {
                    let Some(model) = m.models.get(entity_type) else {
                        continue;
                    };
                    if !super::invariant_eval::all_local_invariants_hold(model, entity_state) {
                        return false;
                    }
                }
                true
            },
        )]
    }
}
