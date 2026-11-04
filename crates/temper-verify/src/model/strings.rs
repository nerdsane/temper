//! String choices and canonical equality symbols.

use super::expansion::Expansion;
use super::heap::{Heap, whole};
use super::types::{
    ModelString, ParamValue, ResolvedTransition, StringParam, TemperModel, TemperModelState,
};
use std::collections::{BTreeMap, BTreeSet};

/// The scalar strings and list members held by `states`, `''` left out.
pub fn string_values<'a>(
    states: impl IntoIterator<Item = &'a TemperModelState>,
) -> BTreeSet<ModelString> {
    states
        .into_iter()
        .flat_map(|state| state.strings.values().chain(state.lists.values().flatten()))
        .filter(|value| !value.is_empty())
        .cloned()
        .collect()
}

/// A value equal to none of `values`, preserving whether it is blank.
pub(crate) fn fresh_string(values: &BTreeSet<ModelString>, blank: bool) -> ModelString {
    let largest = values
        .iter()
        .filter_map(ModelString::symbol_id)
        .max()
        .unwrap_or(0);
    if blank {
        ModelString::BlankSymbol(largest + 1)
    } else {
        ModelString::Symbol(largest + 1)
    }
}

/// The values one string parameter may take in `state`.
fn string_candidates(
    model: &TemperModel,
    param: &StringParam,
    state: &TemperModelState,
    values: &BTreeSet<ModelString>,
) -> Vec<ModelString> {
    let tracked = |var: &str| state.strings.get(var).cloned();
    let mut candidates: BTreeSet<ModelString> =
        match param.equals.iter().find_map(|var| tracked(var)) {
            Some(value) => BTreeSet::from([value]),
            None => {
                let mut all: BTreeSet<ModelString> = values.clone();
                if model.literals_by_class {
                    let own = model.string_literals.get(&param.name);
                    all.retain(|value| match value {
                        ModelString::Literal(text) => own.is_some_and(|own| own.contains(text)),
                        _ => true,
                    });
                }
                all.insert("".into());
                all.insert(fresh_string(values, false));
                all.insert(fresh_string(values, true));
                if let Some(literals) = model.string_literals.get(&param.name) {
                    all.extend(literals.iter().cloned().map(ModelString::from));
                }
                all
            }
        };
    // Every equality must hold, including when two fields disagree.
    for var in &param.equals {
        if let Some(value) = tracked(var) {
            candidates.retain(|candidate| candidate == &value);
        }
    }
    if param.nonempty {
        candidates.retain(|candidate| !candidate.is_blank());
    }
    for var in &param.not_equals {
        if let Some(value) = tracked(var) {
            candidates.remove(&value);
        }
    }
    candidates.into_iter().collect()
}

/// Every choice of values for `t`'s string parameters (one empty choice when
/// it has none; none when a constraint leaves a parameter no value).
pub(super) fn string_assignments(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
    values: &BTreeSet<ModelString>,
    budget: &Expansion,
) -> Vec<BTreeMap<String, ModelString>> {
    let mut assignments = Vec::new();
    if !budget.push(&mut assignments, BTreeMap::new()) {
        return assignments;
    }
    for param in &t.string_params {
        let mut more = Vec::new();
        let before = assignments.heap();
        for assignment in assignments {
            if !budget.take() {
                return Vec::new();
            }
            let scope_room = values.heap() + assignment.heap();
            if !budget.hold(scope_room) {
                return Vec::new();
            }
            let mut scope = values.clone();
            scope.extend(assignment.values().cloned());
            let literals = model.string_literals.get(&param.name);
            let candidate_room = scope.heap()
                + literals.map_or(0, Heap::heap)
                + super::heap::tree_bytes::<ModelString, ()>(
                    scope.len() + literals.map_or(0, BTreeSet::len) + 3,
                )
                + (scope.len() + literals.map_or(0, BTreeSet::len) + 3)
                    * whole(&ModelString::Symbol(0));
            if !budget.hold(candidate_room) {
                return Vec::new();
            }
            let mut candidates = string_candidates(model, param, state, &scope);
            // A parameter neither effects nor guard reads leads to the same
            // move with any value its constraints allow: one is enough.
            if !param.writes
                && !t.params.contains_key(&param.name)
                && !t.guard.params_read().contains(&param.name)
            {
                candidates.truncate(1);
            }
            for value in candidates {
                if !budget.take() {
                    return Vec::new();
                }
                let Some(next) = budget.with_entry(&assignment, &param.name, &value) else {
                    return Vec::new();
                };
                if !budget.reserve(&mut more) {
                    return Vec::new();
                }
                more.push(next);
            }
            drop(scope);
            budget.give(scope_room + candidate_room);
        }
        budget.give(before);
        assignments = more;
    }
    assignments
}

/// Write each string parameter that writes a tracked variable into it, as
/// the runtime writes every parameter into the field of its name.
pub fn apply_string_params(
    string_params: &[StringParam],
    state: &mut TemperModelState,
    params: &BTreeMap<String, ParamValue>,
) {
    for param in string_params.iter().filter(|p| p.writes) {
        if let Some(ParamValue::Str(value)) = params.get(&param.name) {
            state.strings.insert(param.name.clone(), value.clone());
        }
    }
}

/// Renumber the `#n` values of `maps`, in order, by first use: two states
/// that differ only in which new values they made up are one state. Equality
/// between values and their blank classification are preserved.
/// Returns each value's new name.
pub fn canonical_strings(
    maps: &mut [&mut BTreeMap<String, ModelString>],
) -> BTreeMap<ModelString, ModelString> {
    canonical_values(maps.iter_mut().flat_map(|map| map.values_mut()))
}

/// Renumber equality symbols across scalar strings and list elements together.
pub fn canonical_state(state: &mut TemperModelState) {
    canonical_values(
        state
            .strings
            .values_mut()
            .chain(state.lists.values_mut().flatten()),
    );
}

/// Renumber a sequence of values, retaining each equality class.
pub fn canonical_values<'a>(
    values: impl IntoIterator<Item = &'a mut ModelString>,
) -> BTreeMap<ModelString, ModelString> {
    let values: Vec<&mut ModelString> = values.into_iter().collect();
    let renamed = canonical_names(values.iter().map(|value| &**value));
    for value in values {
        if let Some(name) = renamed.get(value) {
            value.clone_from(name);
        }
    }
    renamed
}

/// The new name of each symbol among `values`, numbered in order of first
/// use. Literals keep their names and are left out.
pub fn canonical_names<'a>(
    values: impl IntoIterator<Item = &'a ModelString>,
) -> BTreeMap<ModelString, ModelString> {
    let mut renamed = BTreeMap::new();
    for value in values {
        let id = renamed.len() + 1;
        let name = match value {
            ModelString::Literal(_) => continue,
            ModelString::Symbol(_) => ModelString::Symbol(id),
            ModelString::BlankSymbol(_) => ModelString::BlankSymbol(id),
        };
        renamed.entry(value.clone()).or_insert(name);
    }
    renamed
}
