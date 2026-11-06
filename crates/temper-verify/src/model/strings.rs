//! String choices and canonical equality symbols.

use super::expansion::Expansion;
use super::heap::{Heap, whole};
use super::types::{
    Fields, ModelString, ParamValue, ResolvedTransition, StringParam, TemperModel, TemperModelState,
};
use std::collections::BTreeSet;

/// The scalar strings and list members held by `states`, `''` left out.
pub fn string_values<'a>(
    states: impl IntoIterator<Item = &'a TemperModelState>,
) -> BTreeSet<ModelString> {
    states
        .into_iter()
        .flat_map(|state| state.strings.values().chain(state.lists.values().flatten()))
        .filter(|value| !value.is_empty())
        .copied()
        .collect()
}

/// Whether a made-up value of `class` that names no record is the only such
/// value. An odd class is one nothing compares except with a record's id.
pub(crate) fn names_no_record(class: usize) -> bool {
    class % 2 == 1
}

/// A value of `class` equal to none of `values`, blank or not.
pub(crate) fn fresh_string(
    values: &BTreeSet<ModelString>,
    class: usize,
    blank: bool,
) -> ModelString {
    let largest = values
        .iter()
        .filter(|value| value.class() == Some(class))
        .filter_map(ModelString::number)
        .max()
        .unwrap_or(0);
    ModelString::in_class(class, largest + 1, blank)
}

/// The values one string parameter may take in `state`.
fn string_candidates(
    model: &TemperModel,
    param: &StringParam,
    state: &TemperModelState,
    values: &BTreeSet<ModelString>,
) -> Vec<ModelString> {
    let tracked = |var: &str| state.strings.get(var).copied();
    let mut candidates: BTreeSet<ModelString> =
        match param.equals.iter().find_map(|var| tracked(var)) {
            Some(value) => BTreeSet::from([value]),
            None => {
                let class = model.string_class(&param.name);
                // A made-up value of another class is, to everything that
                // reads this parameter, one more fresh value.
                let mut all: BTreeSet<ModelString> = values
                    .iter()
                    .filter(|value| value.class().is_none_or(|of| of == class))
                    .copied()
                    .collect();
                if model.literals_by_class {
                    let own = model.string_literals.get(param.name.as_str());
                    all.retain(|value| match value {
                        ModelString::Literal(text) => {
                            own.is_some_and(|own| own.contains(text.as_str()))
                        }
                        _ => true,
                    });
                }
                all.insert("".into());
                let fresh = |blank| {
                    if names_no_record(class) {
                        ModelString::in_class(class, 0, blank)
                    } else {
                        fresh_string(values, class, blank)
                    }
                };
                all.insert(fresh(false));
                if model.blank_read(class) {
                    all.insert(fresh(true));
                }
                if let Some(literals) = model.string_literals.get(param.name.as_str()) {
                    all.extend(literals.iter().map(|text| ModelString::from(text.as_str())));
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
    if param.caller {
        candidates.retain(|candidate| !candidate.is_empty());
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
) -> Vec<Fields<ModelString>> {
    let mut assignments = Vec::new();
    if !budget.push(&mut assignments, Fields::new()) {
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
            scope.extend(assignment.values().copied());
            let literals = model.string_literals.get(param.name.as_str());
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
                && !t.params.contains_key(param.name.as_str())
                && !t.guard.params_read().iter().any(|p| *p == param.name)
            {
                candidates.truncate(1);
            }
            for value in candidates {
                if !budget.take() {
                    return Vec::new();
                }
                let Some(next) = budget.with_entry(&assignment, param.name, &value) else {
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
    params: &Fields<ParamValue>,
) {
    for param in string_params.iter().filter(|p| p.writes) {
        if let Some(ParamValue::Str(value)) = params.get(&param.name) {
            state.strings.insert(param.name, *value);
        }
    }
}

/// Renumber the `#n` values of `maps`, in order, by first use: two states
/// that differ only in which new values they made up are one state. Equality
/// between values and their blank classification are preserved.
/// Returns each value's new name.
pub fn canonical_strings(maps: &mut [&mut Fields<ModelString>]) -> Renames {
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
pub fn canonical_values<'a>(values: impl IntoIterator<Item = &'a mut ModelString>) -> Renames {
    let values: Vec<&mut ModelString> = values.into_iter().collect();
    let renamed = canonical_names(values.iter().map(|value| &**value));
    for value in values {
        if let Some(name) = renamed.get(value) {
            *value = *name;
        }
    }
    renamed
}

/// The new name of each symbol among `values`, numbered in order of first
/// use. Literals keep their names and are left out.
pub fn canonical_names<'a>(values: impl IntoIterator<Item = &'a ModelString>) -> Renames {
    let mut renamed = Renames::default();
    renamed.name_all(values);
    renamed
}

/// The new name of each symbol a state holds: as many as the state's distinct
/// made-up values, so few that a list is the quickest way to look one up.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Renames(Vec<(ModelString, ModelString)>);

impl Renames {
    /// Number the symbols among `values`, which are record ids, the list does
    /// not have yet, each carrying on from the last of its class.
    pub fn name_ids<'a>(&mut self, values: impl IntoIterator<Item = &'a ModelString>) {
        self.name(values, false);
    }

    /// As [`Self::name_ids`] for any other values, given the record ids first:
    /// one that names no record, in a class where those are one value, gets
    /// that value.
    pub fn name_all<'a>(&mut self, values: impl IntoIterator<Item = &'a ModelString>) {
        self.name(values, true);
    }

    fn name<'a>(&mut self, values: impl IntoIterator<Item = &'a ModelString>, ids_named: bool) {
        for value in values {
            let Some(class) = value.class() else {
                continue;
            };
            if self.get(value).is_none() {
                if ids_named && names_no_record(class) {
                    self.0
                        .push((*value, ModelString::in_class(class, 0, value.is_blank())));
                    continue;
                }
                let same_class =
                    |(old, _): &&(ModelString, ModelString)| old.class() == Some(class);
                let number = self.0.iter().filter(same_class).count() + 1;
                self.0.push((
                    *value,
                    ModelString::in_class(class, number, value.is_blank()),
                ));
            }
        }
    }

    /// The new name of `value`, when it is a symbol the state holds.
    pub fn get(&self, value: &ModelString) -> Option<&ModelString> {
        self.0
            .iter()
            .find(|(old, _)| old == value)
            .map(|(_, new)| new)
    }

    /// Whether a symbol changes its name.
    pub fn any_moved(&self) -> bool {
        self.0.iter().any(|(old, new)| old != new)
    }

    /// Each symbol and its new name, in order of first use.
    pub fn iter(&self) -> impl Iterator<Item = (&ModelString, &ModelString)> {
        self.0.iter().map(|(old, new)| (old, new))
    }

    /// A list from symbols and their new names.
    pub fn from_pairs(pairs: impl IntoIterator<Item = (ModelString, ModelString)>) -> Self {
        Self(pairs.into_iter().collect())
    }
}

impl Heap for Renames {
    fn heap(&self) -> usize {
        match self.0.capacity() * size_of::<(ModelString, ModelString)>() {
            0 => 0,
            bytes => super::heap::block(bytes),
        }
    }
}
