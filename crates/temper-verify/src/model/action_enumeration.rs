//! Finite action enumeration for local and joint identity namespaces.

use std::collections::BTreeMap;

use super::reference_contract::{
    finite_assignments, fresh_references, guard_may_hold_with_params, parameter_budgets,
};
use super::types::{ModelEffect, TemperModel, TemperModelAction, TemperModelState};

pub(crate) fn enumerate_actions(
    model: &TemperModel,
    state: &TemperModelState,
    joint_slots: Option<&BTreeMap<String, usize>>,
    actions: &mut Vec<TemperModelAction>,
) {
    for transition in &model.transitions {
        let status_ok = transition.from_states.is_empty()
            || transition
                .from_states
                .iter()
                .any(|status| status == &state.status);
        if !status_ok || !effects_within_bounds(model, state, &transition.effects) {
            continue;
        }
        let budgets = joint_slots.map_or_else(
            || parameter_budgets(&transition.effects),
            |slots| joint_parameter_budgets(&transition.effects, slots),
        );
        for reference_params in finite_assignments(&budgets) {
            if reference_params.values().any(|symbol| *symbol == 0)
                || !guard_may_hold_with_params(&transition.guard, state, &reference_params)
            {
                continue;
            }
            actions.push(TemperModelAction {
                name: transition.name.clone(),
                target_state: transition.to_state.clone(),
                reference_params,
                fresh_references: joint_slots.map_or_else(
                    || fresh_references(&transition.effects),
                    |slots| joint_fresh_references(&transition.effects, slots),
                ),
            });
        }
    }
}

fn effects_within_bounds(
    model: &TemperModel,
    state: &TemperModelState,
    effects: &[ModelEffect],
) -> bool {
    effects.iter().all(|effect| match effect {
        ModelEffect::IncrementCounter(var) => {
            let current = state.counters.get(var).copied().unwrap_or(0);
            let bound = model
                .counter_bounds
                .get(var)
                .copied()
                .unwrap_or(model.default_max_counter);
            current < bound
        }
        ModelEffect::ListAppend(var) => {
            state.lists.get(var).map_or(0, Vec::len) < model.default_max_counter
        }
        _ => true,
    })
}

fn joint_parameter_budgets(
    effects: &[ModelEffect],
    joint_slots: &BTreeMap<String, usize>,
) -> Vec<(String, usize)> {
    let mut parameters = Vec::new();
    let mut params_by_type = BTreeMap::<String, usize>::new();
    let mut fresh_by_type = BTreeMap::<String, usize>::new();
    for effect in effects {
        match effect {
            ModelEffect::ExploreReferenceParam { param, entity_type } => {
                parameters.push((param.clone(), entity_type.clone()));
                *params_by_type.entry(entity_type.clone()).or_default() += 1;
            }
            ModelEffect::ReserveFreshReferences { entity_type, count } => {
                fresh_by_type.insert(entity_type.clone(), *count);
            }
            _ => {}
        }
    }
    parameters
        .into_iter()
        .map(|(param, entity_type)| {
            let budget = joint_slots
                .get(&entity_type)
                .copied()
                .unwrap_or(0)
                .saturating_add(params_by_type.get(&entity_type).copied().unwrap_or(0))
                .saturating_add(fresh_by_type.get(&entity_type).copied().unwrap_or(0));
            (param, budget)
        })
        .collect()
}

fn joint_fresh_references(
    effects: &[ModelEffect],
    joint_slots: &BTreeMap<String, usize>,
) -> BTreeMap<String, Vec<u8>> {
    let mut params_by_type = BTreeMap::<String, usize>::new();
    let mut fresh_by_type = BTreeMap::<String, usize>::new();
    for effect in effects {
        match effect {
            ModelEffect::ExploreReferenceParam { entity_type, .. } => {
                *params_by_type.entry(entity_type.clone()).or_default() += 1;
            }
            ModelEffect::ReserveFreshReferences { entity_type, count } => {
                fresh_by_type.insert(entity_type.clone(), *count);
            }
            _ => {}
        }
    }
    fresh_by_type
        .into_iter()
        .map(|(entity_type, count)| {
            let first = joint_slots
                .get(&entity_type)
                .copied()
                .unwrap_or(0)
                .saturating_add(params_by_type.get(&entity_type).copied().unwrap_or(0))
                .saturating_add(1);
            let symbols = (first..first.saturating_add(count))
                .map(|symbol| symbol.min(u8::MAX as usize) as u8)
                .collect();
            (entity_type, symbols)
        })
        .collect()
}
