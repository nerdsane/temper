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
pub(super) const MAX_TRIGGER_DEPTH: u32 = 8;

/// A cross-entity reaction that was dropped because its target action was
/// not enabled from the target entity's state at dispatch time (ADR-0150).
///
/// "Dropped" means: a `kind = "entity"` trigger fired post-commit, but the
/// target entity was not in a state (or its guard was false) where the
/// target action could run — so the intended state change never happened.
/// `create`-resolver reactions and `drop_ok = true` triggers never produce
/// this record (they are exempt / intentionally best-effort).
#[derive(
    Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct DroppedReaction {
    /// Entity whose action fired the reaction.
    pub source_entity: String,
    /// Action on the source entity that fired the reaction.
    pub source_action: String,
    /// Trigger declaration name (from the spec).
    pub trigger_name: String,
    /// Entity the reaction targeted.
    pub target_entity: String,
    /// Action the reaction tried to dispatch on the target.
    pub target_action: String,
    /// The target entity's status at the moment the reaction was dropped.
    pub target_state: String,
}

impl fmt::Display for DroppedReaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}.{} -[{}]-> {}.{} dropped (target in '{}', action not enabled)",
            self.source_entity,
            self.source_action,
            self.trigger_name,
            self.target_entity,
            self.target_action,
            self.target_state,
        )
    }
}

/// Joint state over a composition of entities.
///
/// `BTreeMap` rather than `HashMap` for deterministic iteration order
/// (DST requirement — Stateright's BFS must produce identical state
/// enumerations across seeded runs).
#[derive(Clone, Debug, Hash, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CompositeState {
    /// Per-entity state keyed by entity type name.
    pub entities: BTreeMap<String, TemperModelState>,
    /// The first reaction dropped on the transition that produced this
    /// state, if any (ADR-0150). `None` for the initial state and for any
    /// transition where every fired reaction landed on an enabled target.
    /// Carried in the state (not just returned) so Stateright's
    /// `Property::always` — which sees only `&State` — can flag it and
    /// produce a counterexample path naming the drop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dropped: Option<DroppedReaction>,
}

impl fmt::Display for CompositeState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self
            .entities
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        write!(f, "[{}]", parts.join(" | "))?;
        if let Some(dropped) = &self.dropped {
            write!(f, " !! DROPPED: {dropped}")?;
        }
        Ok(())
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
    /// Deterministic branch through the finite reaction-parameter product.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub cascade_branch: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl fmt::Display for CompositeAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.entity, self.action)?;
        if self.cascade_branch != 0 {
            write!(f, "#{}", self.cascade_branch)?;
        }
        Ok(())
    }
}

/// Joint-state model: composes per-entity [`TemperModel`]s into a
/// Stateright-checkable product automaton with cross-entity trigger
/// cascades.
#[derive(Clone)]
pub struct CompositeTemperModel {
    /// Per-entity models keyed by entity type name.
    pub(super) models: BTreeMap<String, TemperModel>,
    /// All entity-kind trigger edges within the composition scope.
    pub(super) edges: Vec<TriggerEdge>,
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

    /// Enumerate **every distinct** dropped reaction reachable in the joint
    /// state space (ADR-0150), bounded by `state_budget` unique states.
    ///
    /// Stateright's `no_dropped_reaction` property reports only one
    /// counterexample (the first witnessed drop) — useful for a minimal
    /// trace, but a spec can contain several independent dropped reactions
    /// that an author wants to see all at once. This walks the reachable
    /// joint states (a manual BFS mirroring [`crate::checker`]'s
    /// dead-transition pass) and collects each distinct drop.
    ///
    /// Returns `(drops, complete)`: `complete` is `false` if the budget was
    /// hit before the space was exhausted (the drop list is then partial).
    /// Distinctness is by the full [`DroppedReaction`] tuple, so the same
    /// logical drop reached via different paths is reported once.
    pub fn enumerate_drops(&self, state_budget: usize) -> (Vec<DroppedReaction>, bool) {
        use std::collections::BTreeSet;
        use std::collections::VecDeque;

        let mut drops: BTreeSet<DroppedReaction> = BTreeSet::new();
        // The `dropped` marker is a per-transition artifact, not part of
        // entity identity, so the visited-set key renders only the entities
        // (via Display); two states differing only in their drop marker
        // collapse to one key.
        let mut visited: BTreeSet<String> = BTreeSet::new();
        let mut queue: VecDeque<CompositeState> = VecDeque::new();

        for init in self.init_states() {
            if visited.insert(state_key(&init)) {
                queue.push_back(init);
            }
        }

        let mut complete = true;
        while let Some(state) = queue.pop_front() {
            if visited.len() >= state_budget {
                complete = false;
                break;
            }
            let mut actions = Vec::new();
            self.actions(&state, &mut actions);
            for action in actions {
                let Some(next) = self.next_state(&state, action) else {
                    continue;
                };
                if let Some(drop) = &next.dropped {
                    drops.insert(drop.clone());
                }
                if visited.insert(state_key(&next)) {
                    queue.push_back(next);
                }
            }
        }

        (drops.into_iter().collect(), complete)
    }
}

/// Visited-set key for a joint state: the per-entity rendering, with the
/// per-transition `dropped` marker excluded (it is not part of identity).
pub(super) fn state_key(state: &CompositeState) -> String {
    let parts: Vec<String> = state
        .entities
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect();
    parts.join(" | ")
}

impl fmt::Debug for CompositeTemperModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompositeTemperModel")
            .field("entity_types", &self.entity_types)
            .field("edges", &self.edges.len())
            .finish()
    }
}

impl Model for CompositeTemperModel {
    type State = CompositeState;
    type Action = CompositeAction;

    fn init_states(&self) -> Vec<Self::State> {
        let mut products = vec![BTreeMap::new()];
        for (entity_type, model) in &self.models {
            let mut expanded = Vec::new();
            for product in &products {
                for initial in model.init_states() {
                    let mut next = product.clone();
                    next.insert(entity_type.clone(), initial);
                    expanded.push(next);
                }
            }
            products = expanded;
        }
        let mut unique = BTreeMap::new();
        for state in products.into_iter().flat_map(|entities| {
            self.expand_joint_reference_patterns(CompositeState {
                entities,
                dropped: None,
            })
        }) {
            unique.insert(state_key(&state), state);
        }
        unique.into_values().collect()
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
            crate::model::action_enumeration::enumerate_actions(
                entity_model,
                entity_state,
                Some(&self.joint_reference_slots()),
                &mut per_entity,
            );
            for a in per_entity {
                // The per-entity model offers cross-entity-guarded actions
                // unconditionally (the guard is abstract locally). In the
                // joint model the referenced entity is in scope, so resolve
                // the guard concretely and drop the action when it is false.
                if !self.cross_entity_guards_satisfied(entity_type, &a.name, state) {
                    continue;
                }
                let Some(new_source_state) =
                    entity_model.next_state_preserving_references(entity_state, a.clone())
                else {
                    continue;
                };
                let mut post_source = state.clone();
                post_source
                    .entities
                    .insert(entity_type.clone(), new_source_state.clone());
                let branch_count = self
                    .post_action_successors(
                        post_source,
                        entity_type,
                        &a.name,
                        &new_source_state.status,
                    )
                    .len();
                for cascade_branch in 0..branch_count {
                    out.push(CompositeAction {
                        entity: entity_type.clone(),
                        action: a.clone(),
                        cascade_branch,
                    });
                }
            }
        }
    }

    fn next_state(&self, state: &Self::State, action: Self::Action) -> Option<Self::State> {
        let source_model = self.models.get(&action.entity)?;
        let source_state = state.entities.get(&action.entity)?;

        // Defensive: never apply a source action whose cross-entity guard is
        // false in the joint state (mirrors the filter in `actions`).
        if !self.cross_entity_guards_satisfied(&action.entity, &action.action.name, state) {
            return None;
        }

        // Apply the primary (external) action to the actor entity.
        let new_source_state =
            source_model.next_state_preserving_references(source_state, action.action.clone())?;
        let new_source_status = new_source_state.status.clone();

        // Commit to joint state. Clear any drop recorded on the predecessor —
        // `dropped` describes only the reactions fired on THIS transition, so
        // each successor starts clean and the cascade below repopulates it.
        let mut next = state.clone();
        next.dropped = None;
        next.entities
            .insert(action.entity.clone(), new_source_state);

        // Fire-and-forget cascade: apply triggered transitions
        // transitively, bounded by MAX_TRIGGER_DEPTH. Records the first
        // dropped reaction (if any) into `next.dropped`.
        let successors = self.post_action_successors(
            next,
            &action.entity,
            &action.action.name,
            &new_source_status,
        );
        successors.get(action.cascade_branch).cloned()
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
        vec![
            Property::<Self>::always("joint_local_invariants", |m, s| {
                for (entity_type, entity_state) in &s.entities {
                    let Some(model) = m.models.get(entity_type) else {
                        continue;
                    };
                    if !super::invariant_eval::all_local_invariants_hold(model, entity_state) {
                        return false;
                    }
                }
                true
            }),
            // ADR-0150: no cross-entity reaction is dropped because its target
            // was not in the required state. `next_state` records the first
            // such drop into `s.dropped`; a reachable state carrying a drop is
            // a counterexample. `create`-resolver and `drop_ok` reactions are
            // exempt (never recorded), so they never trip this property.
            Property::<Self>::always("no_dropped_reaction", |_m, s| s.dropped.is_none()),
        ]
    }
}

#[cfg(test)]
#[path = "model_test.rs"]
mod tests;
