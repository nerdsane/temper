//! Finite typed-reference identity abstraction for the Stateright backend.

use std::collections::BTreeMap;

use temper_spec::automaton::Automaton;

use super::semantics::evaluate_guard;
use super::types::{ModelEffect, ModelGuard, TemperModel, TemperModelAction, TemperModelState};

impl TemperModel {
    pub(crate) fn next_state_preserving_references(
        &self,
        state: &TemperModelState,
        action: TemperModelAction,
    ) -> Option<TemperModelState> {
        let resolved = self.transitions.iter().find(|t| t.name == action.name)?;
        let mut next = state.clone();
        next.status = action
            .target_state
            .clone()
            .unwrap_or_else(|| state.status.clone());
        super::semantics::apply_effects(&resolved.effects, &mut next, &action.name);
        if !apply_effects_preserving_symbols(&resolved.effects, &mut next, &action.reference_params)
        {
            return None;
        }
        Some(next)
    }
}

pub(super) struct ReferenceModelMetadata {
    reference_properties: std::collections::BTreeSet<String>,
    references_by_type: BTreeMap<String, Vec<String>>,
    identity_properties: Option<Vec<String>>,
    action_params: BTreeMap<String, Vec<(String, String)>>,
    action_fresh: BTreeMap<String, BTreeMap<String, usize>>,
}

impl ReferenceModelMetadata {
    pub(super) fn new(automaton: &Automaton) -> Self {
        let mut reference_properties = std::collections::BTreeSet::new();
        let mut references_by_type = BTreeMap::<String, Vec<String>>::new();
        for state in automaton
            .state
            .iter()
            .filter(|state| state.var_type == "ref")
        {
            reference_properties.insert(state.name.clone());
            references_by_type
                .entry(state.entity_type.clone().unwrap_or_default())
                .or_default()
                .push(state.name.clone());
        }
        let identity_properties = automaton
            .keys
            .iter()
            .find(|key| key.entity_id)
            .map(|key| key.properties.clone());
        let mut action_params = BTreeMap::new();
        let mut action_fresh = BTreeMap::new();
        for action in &automaton.actions {
            action_params.insert(
                action.name.clone(),
                action
                    .params
                    .iter()
                    .filter(|param| param.param_type() == "ref")
                    .map(|param| {
                        (
                            param.name().to_string(),
                            param.entity_type().unwrap_or_default().to_string(),
                        )
                    })
                    .collect(),
            );
            let mut fresh = BTreeMap::<String, usize>::new();
            for write in action
                .sub_writes
                .iter()
                .filter(|write| write.action.eq_ignore_ascii_case("create"))
            {
                *fresh.entry(write.target_entity.clone()).or_default() += 1;
            }
            action_fresh.insert(action.name.clone(), fresh);
        }
        Self {
            reference_properties,
            references_by_type,
            identity_properties,
            action_params,
            action_fresh,
        }
    }

    pub(super) fn augment_effects(&self, action: &str, effects: &mut Vec<ModelEffect>) {
        if let Some(params) = self.action_params.get(action) {
            for (param, entity_type) in params {
                effects.push(ModelEffect::ExploreReferenceParam {
                    param: param.clone(),
                    entity_type: entity_type.clone(),
                });
                if self.reference_properties.contains(param) {
                    effects.push(ModelEffect::SetReferenceFromParam {
                        reference: param.clone(),
                        param: param.clone(),
                    });
                }
            }
        }
        if let Some(fresh) = self.action_fresh.get(action) {
            effects.extend(fresh.iter().map(|(entity_type, count)| {
                ModelEffect::ReserveFreshReferences {
                    entity_type: entity_type.clone(),
                    count: *count,
                }
            }));
        }
        if let Some(properties) = &self.identity_properties {
            effects.push(ModelEffect::EnforceIdentity(properties.clone()));
        }
        if !self.references_by_type.is_empty() {
            effects.push(ModelEffect::CanonicalizeReferences(
                self.references_by_type.clone(),
            ));
        }
    }
}

pub(super) fn initialize_reference_counters(
    automaton: &Automaton,
    counters: &mut BTreeMap<String, usize>,
) -> Vec<BTreeMap<String, usize>> {
    for state in automaton
        .state
        .iter()
        .filter(|state| state.var_type == "ref")
    {
        counters.insert(format!("__ref:{}", state.name), 0);
    }
    let Some(identity_key) = automaton.keys.iter().find(|key| key.entity_id) else {
        return vec![counters.clone()];
    };
    let targets = automaton
        .state
        .iter()
        .filter_map(|state| {
            state
                .entity_type
                .as_ref()
                .map(|target| (state.name.as_str(), target.as_str()))
        })
        .collect::<BTreeMap<_, _>>();
    let mut groups = BTreeMap::<&str, Vec<&String>>::new();
    for property in &identity_key.properties {
        groups
            .entry(targets.get(property.as_str()).copied().unwrap_or_default())
            .or_default()
            .push(property);
    }
    let mut variants = vec![counters.clone()];
    for properties in groups.values() {
        let patterns = canonical_partitions(properties.len());
        let mut expanded = Vec::new();
        for variant in &variants {
            for pattern in &patterns {
                let mut next = variant.clone();
                for (property, symbol) in properties.iter().zip(pattern) {
                    next.insert(format!("__ref:{property}"), *symbol);
                }
                expanded.push(next);
            }
        }
        variants = expanded;
    }
    for variant in &mut variants {
        for (index, property) in identity_key.properties.iter().enumerate() {
            let symbol = variant
                .get(&format!("__ref:{property}"))
                .copied()
                .unwrap_or(0);
            variant.insert(format!("__id:{index}"), symbol);
        }
    }
    variants
}

pub(crate) fn canonical_partitions(len: usize) -> Vec<Vec<usize>> {
    fn extend(prefix: &mut Vec<usize>, len: usize, out: &mut Vec<Vec<usize>>) {
        if prefix.len() == len {
            out.push(prefix.clone());
            return;
        }
        let max = prefix.iter().copied().max().unwrap_or(0);
        for symbol in 1..=max.saturating_add(1).max(1) {
            prefix.push(symbol);
            extend(prefix, len, out);
            prefix.pop();
        }
    }
    let mut out = Vec::new();
    extend(&mut Vec::new(), len, &mut out);
    out
}

pub(crate) fn guard_may_hold_with_params(
    guard: &ModelGuard,
    state: &TemperModelState,
    params: &BTreeMap<String, u8>,
) -> bool {
    match guard {
        ModelGuard::CrossEntityState { .. } => true,
        ModelGuard::ReferenceEquals { reference, param } => {
            let stored = state
                .counters
                .get(&format!("__ref:{reference}"))
                .copied()
                .unwrap_or(0) as u8;
            stored != 0 && Some(&stored) == params.get(param)
        }
        ModelGuard::And(guards) => guards
            .iter()
            .all(|guard| guard_may_hold_with_params(guard, state, params)),
        _ => evaluate_guard(guard, state),
    }
}

pub(crate) fn parameter_budgets(effects: &[ModelEffect]) -> Vec<(String, usize)> {
    let mut stored_by_type = BTreeMap::new();
    let mut fresh_by_type = BTreeMap::new();
    let mut parameters = Vec::new();
    for effect in effects {
        match effect {
            ModelEffect::ExploreReferenceParam { param, entity_type } => {
                parameters.push((param.clone(), entity_type.clone()));
            }
            ModelEffect::CanonicalizeReferences(groups) => {
                for (entity_type, properties) in groups {
                    stored_by_type.insert(entity_type.clone(), properties.len());
                }
            }
            ModelEffect::ReserveFreshReferences { entity_type, count } => {
                fresh_by_type.insert(entity_type.clone(), *count);
            }
            _ => {}
        }
    }
    let mut params_by_type = BTreeMap::<String, usize>::new();
    for (_, entity_type) in &parameters {
        *params_by_type.entry(entity_type.clone()).or_default() += 1;
    }
    parameters
        .into_iter()
        .map(|(param, entity_type)| {
            let budget = stored_by_type
                .get(&entity_type)
                .copied()
                .unwrap_or(0)
                .saturating_add(params_by_type.get(&entity_type).copied().unwrap_or(0))
                .saturating_add(fresh_by_type.get(&entity_type).copied().unwrap_or(0));
            (param, budget)
        })
        .collect()
}

pub(crate) fn fresh_references(effects: &[ModelEffect]) -> BTreeMap<String, Vec<u8>> {
    let mut stored_by_type = BTreeMap::<String, usize>::new();
    let mut params_by_type = BTreeMap::<String, usize>::new();
    let mut fresh_by_type = BTreeMap::<String, usize>::new();
    for effect in effects {
        match effect {
            ModelEffect::ExploreReferenceParam { entity_type, .. } => {
                *params_by_type.entry(entity_type.clone()).or_default() += 1;
            }
            ModelEffect::CanonicalizeReferences(groups) => {
                for (entity_type, properties) in groups {
                    stored_by_type.insert(entity_type.clone(), properties.len());
                }
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
            let first = stored_by_type
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

pub(super) fn finite_assignments(params: &[(String, usize)]) -> Vec<BTreeMap<String, u8>> {
    let mut assignments = vec![BTreeMap::new()];
    for (param, class_count) in params {
        let mut expanded = Vec::new();
        for assignment in &assignments {
            for class in 0..=(*class_count).min(u8::MAX as usize) {
                let mut next = assignment.clone();
                next.insert(param.clone(), class as u8);
                expanded.push(next);
            }
        }
        assignments = expanded;
    }
    assignments
}

/// Apply immutable reference projection and deterministic identity binding.
pub(super) fn apply_effects(
    effects: &[ModelEffect],
    state: &mut TemperModelState,
    params: &BTreeMap<String, u8>,
) -> bool {
    apply_effects_with_canonicalization(effects, state, params, true)
}

pub(crate) fn apply_effects_preserving_symbols(
    effects: &[ModelEffect],
    state: &mut TemperModelState,
    params: &BTreeMap<String, u8>,
) -> bool {
    apply_effects_with_canonicalization(effects, state, params, false)
}

fn apply_effects_with_canonicalization(
    effects: &[ModelEffect],
    state: &mut TemperModelState,
    params: &BTreeMap<String, u8>,
    canonicalize: bool,
) -> bool {
    for effect in effects {
        let ModelEffect::SetReferenceFromParam { reference, param } = effect else {
            continue;
        };
        let symbol = params.get(param).copied().unwrap_or(0);
        let key = format!("__ref:{reference}");
        let stored = state.counters.get(&key).copied().unwrap_or(0) as u8;
        if stored != 0 && stored != symbol {
            return false;
        }
        state.counters.insert(key, symbol as usize);
    }
    for effect in effects {
        if let ModelEffect::EnforceIdentity(properties) = effect {
            for (index, property) in properties.iter().enumerate() {
                let symbol = state
                    .counters
                    .get(&format!("__ref:{property}"))
                    .copied()
                    .unwrap_or(0);
                if symbol == 0 {
                    return false;
                }
                let binding_key = format!("__id:{index}");
                let bound = state.counters.get(&binding_key).copied().unwrap_or(0);
                if bound != 0 && bound != symbol {
                    return false;
                }
                state.counters.insert(binding_key, symbol);
            }
        }
    }
    if !canonicalize {
        return true;
    }
    for effect in effects {
        let ModelEffect::CanonicalizeReferences(properties_by_type) = effect else {
            continue;
        };
        for properties in properties_by_type.values() {
            let mut canonical = BTreeMap::new();
            let mut next_symbol = 1usize;
            for property in properties {
                let key = format!("__ref:{property}");
                let symbol = state.counters.get(&key).copied().unwrap_or(0);
                if symbol == 0 {
                    continue;
                }
                let normalized = *canonical.entry(symbol).or_insert_with(|| {
                    let assigned = next_symbol;
                    next_symbol = next_symbol.saturating_add(1);
                    assigned
                });
                state.counters.insert(key, normalized);
            }
        }
    }
    for effect in effects {
        if let ModelEffect::EnforceIdentity(properties) = effect {
            for (index, property) in properties.iter().enumerate() {
                let symbol = state
                    .counters
                    .get(&format!("__ref:{property}"))
                    .copied()
                    .unwrap_or(0);
                let binding_key = format!("__id:{index}");
                state.counters.insert(binding_key, symbol);
            }
        }
    }
    true
}
