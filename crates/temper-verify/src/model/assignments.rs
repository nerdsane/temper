//! Numeric and list input assignments, bounded while they are built.

use super::count_bounds::{CountRange, count_ranges};
use super::expansion::Expansion;
use super::heap::{Heap, block, tree_bytes};
use super::semantics::MAX_PARAM_ASSIGNMENTS;
use super::types::{ModelEffect, ParamValue, ResolvedTransition, TemperModel};
use std::collections::{BTreeMap, BTreeSet};
use temper_spec::predicate::{Arg, ParamKind};

fn param_candidates(
    model: &TemperModel,
    t: &ResolvedTransition,
    name: &str,
    kind: &ParamKind,
    ranges: &BTreeMap<&str, CountRange>,
) -> Vec<ParamValue> {
    match kind {
        ParamKind::Count => (0..=ranges[name].largest).map(ParamValue::Count).collect(),
        ParamKind::Bool => vec![ParamValue::Bool(false), ParamValue::Bool(true)],
        ParamKind::Str => string_literals(model, t, name)
            .into_iter()
            .map(|s| ParamValue::Str(s.into()))
            .chain(std::iter::once(ParamValue::Fresh))
            .collect(),
    }
}

/// The literals a list `t` appends string parameter `name` to, or removes it
/// from, is compared against: the strings worth choosing for it.
pub(super) fn string_literals(
    model: &TemperModel,
    t: &ResolvedTransition,
    name: &str,
) -> BTreeSet<String> {
    let mut values: BTreeSet<String> = BTreeSet::new();
    for effect in &t.effects {
        if let ModelEffect::ListAppend {
            var,
            value: Arg::Param(p),
        }
        | ModelEffect::ListRemove {
            var,
            value: Arg::Param(p),
        } = effect
            && p == name
            && let Some(literals) = model.list_literals.get(var)
        {
            values.extend(literals.iter().cloned());
        }
    }
    values
}

/// The cartesian product of each parameter's candidate values.
pub(super) fn param_assignments_budgeted(
    model: &TemperModel,
    t: &ResolvedTransition,
    budget: &Expansion,
) -> Vec<BTreeMap<String, ParamValue>> {
    let ranges = count_ranges(model, t);
    let mut assignments = Vec::new();
    if !budget.push(&mut assignments, BTreeMap::new()) {
        return assignments;
    }
    for (name, kind) in &t.params {
        if *kind == ParamKind::Str && t.string_params.iter().any(|p| p.name == *name) {
            continue; // String products, including list inputs, use equality classes.
        }
        let candidate_room = match kind {
            ParamKind::Count => {
                block((ranges[name.as_str()].largest + 1) * size_of::<ParamValue>())
            }
            ParamKind::Bool => block(2 * size_of::<ParamValue>()),
            ParamKind::Str => {
                let literals = model.list_literals.values();
                let count = literals.clone().map(BTreeSet::len).sum::<usize>();
                literals.map(Heap::heap).sum::<usize>()
                    + tree_bytes::<String, ()>(count)
                    + block((count + 1).max(4) * 2 * size_of::<ParamValue>())
            }
        };
        if !budget.hold(candidate_room) {
            return Vec::new();
        }
        let candidates = param_candidates(model, t, name, kind, &ranges);
        assert!(
            assignments.len() * candidates.len() <= MAX_PARAM_ASSIGNMENTS,
            "transition '{}' has more than {MAX_PARAM_ASSIGNMENTS} parameter assignments",
            t.name
        );
        let before = assignments.heap();
        let mut more = Vec::new();
        for assignment in assignments {
            for value in &candidates {
                let Some(next) = budget.with_entry(&assignment, name, value) else {
                    return Vec::new();
                };
                if !budget.reserve(&mut more) {
                    return Vec::new();
                }
                more.push(next);
            }
        }
        drop(candidates);
        budget.give(before + candidate_room);
        assignments = more;
    }
    assignments
}
