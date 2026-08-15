//! Deterministic trigger-cascade and joint reference semantics.

use std::collections::BTreeMap;

use temper_spec::automaton::TriggerEdge;

use crate::model::{ModelGuard, TemperModelAction, TemperModelState};

use super::model::{
    CompositeState, CompositeTemperModel, DroppedReaction, MAX_TRIGGER_DEPTH, state_key,
};

impl CompositeTemperModel {
    pub(super) fn joint_reference_slots(&self) -> BTreeMap<String, usize> {
        let mut slots = BTreeMap::new();
        for model in self.models.values() {
            for (target, properties) in &model.reference_properties_by_type {
                *slots.entry(target.clone()).or_default() += properties.len();
            }
        }
        slots
    }

    pub(super) fn cascade_successors(
        &self,
        state: CompositeState,
        source_entity: &str,
        source_action: &str,
        source_to_state: &str,
        depth: u32,
    ) -> Vec<CompositeState> {
        if depth >= MAX_TRIGGER_DEPTH {
            return vec![state];
        }
        let edges = self
            .edges
            .iter()
            .filter(|edge| edge.from == source_entity && edge.source_action == source_action)
            .collect::<Vec<_>>();
        let mut frontier = vec![state];
        for edge in edges {
            if edge
                .to_state
                .as_ref()
                .is_some_and(|expected| expected != source_to_state)
            {
                continue;
            }
            let Some(target_model) = self.models.get(&edge.to) else {
                continue;
            };
            let mut expanded = Vec::new();
            for current in frontier {
                let Some(existing_target_state) = current.entities.get(&edge.to) else {
                    expanded.push(current);
                    continue;
                };
                let target_state = if edge.creates_target {
                    TemperModelState {
                        status: target_model.initial_status.clone(),
                        counters: target_model.initial_counters.clone(),
                        booleans: target_model.initial_booleans.clone(),
                        lists: target_model.initial_lists.clone(),
                    }
                } else {
                    existing_target_state.clone()
                };
                let mut enabled = Vec::new();
                crate::model::action_enumeration::enumerate_actions(
                    target_model,
                    &target_state,
                    Some(&self.joint_reference_slots()),
                    &mut enabled,
                );
                let mut candidates = BTreeMap::new();
                for mut target_action in enabled.into_iter().filter(|action| {
                    action.name == edge.target_action
                        && self.cross_entity_guards_satisfied(&edge.to, &action.name, &current)
                }) {
                    project_reaction_references(&current, source_entity, edge, &mut target_action);
                    let guard_holds = target_model
                        .transitions
                        .iter()
                        .find(|transition| transition.name == target_action.name)
                        .is_some_and(|transition| {
                            crate::model::reference_contract::guard_may_hold_with_params(
                                &transition.guard,
                                &target_state,
                                &target_action.reference_params,
                            )
                        });
                    if guard_holds {
                        candidates.insert(target_action.to_string(), target_action);
                    }
                }
                let mut landed = false;
                for target_action in candidates.into_values() {
                    let Some(new_target_state) = target_model
                        .next_state_preserving_references(&target_state, target_action.clone())
                    else {
                        continue;
                    };
                    landed = true;
                    let new_target_status = new_target_state.status.clone();
                    let mut next = current.clone();
                    next.entities.insert(edge.to.clone(), new_target_state);
                    expanded.extend(self.cascade_successors(
                        next,
                        &edge.to,
                        &target_action.name,
                        &new_target_status,
                        depth + 1,
                    ));
                }
                if !landed {
                    let mut dropped = current;
                    if !edge.creates_target && !edge.drop_ok && dropped.dropped.is_none() {
                        dropped.dropped = Some(DroppedReaction {
                            source_entity: source_entity.to_string(),
                            source_action: source_action.to_string(),
                            trigger_name: edge.trigger_name.clone(),
                            target_entity: edge.to.clone(),
                            target_action: edge.target_action.clone(),
                            target_state: target_state.status.clone(),
                        });
                    }
                    expanded.push(dropped);
                }
            }
            frontier = expanded;
        }
        frontier
    }

    pub(super) fn expand_joint_reference_patterns(
        &self,
        state: CompositeState,
    ) -> Vec<CompositeState> {
        let mut slots_by_target = BTreeMap::<String, Vec<(String, String)>>::new();
        for (entity_type, model) in &self.models {
            let Some(entity_state) = state.entities.get(entity_type) else {
                continue;
            };
            for (target, properties) in &model.reference_properties_by_type {
                for property in properties {
                    if entity_state
                        .counters
                        .get(&format!("__ref:{property}"))
                        .copied()
                        .unwrap_or(0)
                        != 0
                    {
                        slots_by_target
                            .entry(target.clone())
                            .or_default()
                            .push((entity_type.clone(), property.clone()));
                    }
                }
            }
        }
        let mut variants = vec![state];
        for slots in slots_by_target.values() {
            let mut expanded = Vec::new();
            for variant in &variants {
                for pattern in crate::model::reference_contract::canonical_partitions(slots.len()) {
                    let mut next = variant.clone();
                    for ((entity_type, property), symbol) in slots.iter().zip(pattern) {
                        if let Some(entity) = next.entities.get_mut(entity_type) {
                            entity.counters.insert(format!("__ref:{property}"), symbol);
                        }
                    }
                    expanded.push(next);
                }
            }
            variants = expanded;
        }
        for variant in &mut variants {
            self.refresh_identity_bindings(variant);
        }
        variants
    }

    pub(super) fn post_action_successors(
        &self,
        state: CompositeState,
        entity: &str,
        action: &str,
        status: &str,
    ) -> Vec<CompositeState> {
        let mut unique = BTreeMap::new();
        for successor in self.cascade_successors(state, entity, action, status, 0) {
            let successor = self.canonicalize_joint_references(successor);
            unique.insert(state_key(&successor), successor);
        }
        unique.into_values().collect()
    }

    fn canonicalize_joint_references(&self, mut state: CompositeState) -> CompositeState {
        let mut slots_by_target = BTreeMap::<String, Vec<(String, String)>>::new();
        for (entity_type, model) in &self.models {
            for (target, properties) in &model.reference_properties_by_type {
                for property in properties {
                    slots_by_target
                        .entry(target.clone())
                        .or_default()
                        .push((entity_type.clone(), property.clone()));
                }
            }
        }
        for slots in slots_by_target.values() {
            let mut symbols = BTreeMap::new();
            let mut next_symbol = 1usize;
            for (entity_type, property) in slots {
                let Some(entity) = state.entities.get_mut(entity_type) else {
                    continue;
                };
                let key = format!("__ref:{property}");
                let old = entity.counters.get(&key).copied().unwrap_or(0);
                if old == 0 {
                    continue;
                }
                let canonical = *symbols.entry(old).or_insert_with(|| {
                    let assigned = next_symbol;
                    next_symbol = next_symbol.saturating_add(1);
                    assigned
                });
                entity.counters.insert(key, canonical);
            }
        }
        self.refresh_identity_bindings(&mut state);
        state
    }

    fn refresh_identity_bindings(&self, state: &mut CompositeState) {
        for (entity_type, model) in &self.models {
            let Some(entity) = state.entities.get_mut(entity_type) else {
                continue;
            };
            for (index, property) in model.identity_properties.iter().enumerate() {
                let symbol = entity
                    .counters
                    .get(&format!("__ref:{property}"))
                    .copied()
                    .unwrap_or(0);
                entity.counters.insert(format!("__id:{index}"), symbol);
            }
        }
    }

    pub(super) fn cross_entity_guards_satisfied(
        &self,
        source_entity: &str,
        action_name: &str,
        state: &CompositeState,
    ) -> bool {
        let Some(source_model) = self.models.get(source_entity) else {
            return true;
        };
        let Some(transition) = source_model
            .transitions
            .iter()
            .find(|transition| transition.name == action_name)
        else {
            return true;
        };
        self.guard_cross_entity_ok(&transition.guard, state)
    }

    fn guard_cross_entity_ok(&self, guard: &ModelGuard, state: &CompositeState) -> bool {
        match guard {
            ModelGuard::CrossEntityState {
                entity_type,
                required_status,
                forbidden_status,
                ..
            } => match state.entities.get(entity_type) {
                Some(target) => {
                    let allowed = required_status.is_empty()
                        || required_status
                            .iter()
                            .any(|status| status == &target.status);
                    let not_forbidden = !forbidden_status
                        .iter()
                        .any(|status| status == &target.status);
                    allowed && not_forbidden
                }
                None => true,
            },
            ModelGuard::And(guards) => guards
                .iter()
                .all(|guard| self.guard_cross_entity_ok(guard, state)),
            _ => true,
        }
    }
}

fn project_reaction_references(
    state: &CompositeState,
    source_entity: &str,
    edge: &TriggerEdge,
    target_action: &mut TemperModelAction,
) {
    let Some(source) = state.entities.get(source_entity) else {
        return;
    };
    for (target_param, source_field) in &edge.params_from {
        if let Some(symbol) = source.counters.get(&format!("__ref:{source_field}")) {
            target_action
                .reference_params
                .insert(target_param.clone(), *symbol as u8);
        }
    }
}
