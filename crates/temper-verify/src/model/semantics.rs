//! Shared concrete guard/effect semantics for verification backends.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::predicate::{
    Arg, Env, Expr, Literal, Operand, ParamKind, Set, Truth, Val, VarKind, eval,
};

use super::types::{
    ModelEffect, ModelString, ParamValue, ResolvedTransition, TemperModel, TemperModelState,
};

use super::count_bounds::{CountRange, count_ranges};
pub(crate) use super::expansion::Expansion;
use super::parameters::{any_inputs, checks_inputs, inputs_allow};
pub use super::strings::{
    apply_string_params, canonical_state, canonical_strings, canonical_values, string_values,
};

/// Parameter assignments explored per step for one transition, at most
/// (numeric/list parameters; joint string products also consume expansion work).
pub const MAX_PARAM_ASSIGNMENTS: usize = 4096;

/// A model state as a predicate environment. Status, counters, booleans,
/// lists and tracked strings are modeled; declared counters, booleans and
/// lists missing from the state read as their zero value. Untracked string
/// and numeric variables, entity fields and related entities are not modeled
/// and read as unknown.
pub struct ModelEnv<'a> {
    /// Declared state variable types.
    pub kinds: &'a BTreeMap<String, VarKind>,
    /// The state being evaluated.
    pub state: &'a TemperModelState,
}

impl Env for ModelEnv<'_> {
    fn status(&self) -> Val<'_> {
        Val::Str(&self.state.status)
    }

    fn var(&self, name: &str) -> Val<'_> {
        match self.kinds.get(name) {
            Some(VarKind::Counter) => {
                Val::Num(self.state.counters.get(name).copied().unwrap_or(0) as f64)
            }
            Some(VarKind::Bool) => {
                Val::Bool(self.state.booleans.get(name).copied().unwrap_or(false))
            }
            Some(VarKind::List) => self.state.lists.get(name).map_or_else(
                || Val::Values(Vec::new()),
                |items| Val::Values(items.iter().map(|s| s.val()).collect()),
            ),
            Some(VarKind::Str) => self
                .state
                .strings
                .get(name)
                .map_or(Val::Unknown, |value| value.val()),
            Some(VarKind::Num) | None => Val::Unknown,
        }
    }

    fn cross_statuses(&self, _: &str, _: &str) -> Option<Vec<Val<'_>>> {
        None
    }
}

/// Three-valued truth of `expr` in `state`.
pub fn truth(expr: &Expr, kinds: &BTreeMap<String, VarKind>, state: &TemperModelState) -> Truth {
    eval(expr, &ModelEnv { kinds, state })
}

/// Whether a guard is **guaranteed** to hold from the local state alone.
///
/// Used by local-enablement checks (`terminal`, `no_deadlock`): a transition
/// gated on something the model cannot see (a related entity's status) is
/// not locally enabled, so a state whose only exit is such a gate is correctly
/// treated as a place the automaton may wait.
pub fn evaluate_guard(
    guard: &Expr,
    kinds: &BTreeMap<String, VarKind>,
    state: &TemperModelState,
) -> bool {
    truth(guard, kinds, state).must_hold()
}

/// Whether a guard **may** hold, for state-space exploration.
///
/// Unknown values are free (nondeterministic) booleans (ADR-0149): the edge
/// is offered when the guard could hold (guard-true branch); the BFS covers the
/// guard-false branch by also exploring states where the edge is not taken.
/// Locally resolvable parts still decide: an unsatisfiable local condition
/// keeps the guard false whatever the unknowns are.
pub fn guard_may_hold(
    guard: &Expr,
    kinds: &BTreeMap<String, VarKind>,
    state: &TemperModelState,
) -> bool {
    truth(guard, kinds, state).may_hold()
}

/// Every state `t` can step to from `state`, with the parameter values
/// chosen for it. The caller has checked `t`'s status and guard.
///
/// A parameter is an unknown value, so each candidate is explored:
/// counts up to as far as the move's effects can carry them inside the
/// bounds (`count_bounds`), both booleans, and for list elements held values,
/// relevant literals and concrete fresh equality symbols.
/// Assignments that grow a counter or a list past its bound are pruned, as
/// in bounded exploration.
pub fn successors(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
) -> Vec<(BTreeMap<String, ParamValue>, TemperModelState)> {
    successors_among(model, t, state, &string_values(std::iter::once(state)))
}

/// [`successors`], with `values` the tracked strings in scope: the state's
/// own, or every entity's in a joint state. A string parameter may take any
/// of them (a value copied from somewhere), `''`, a literal the spec
/// compares its variable with, or one value new to the scope, as far as the
/// action's constraints allow.
pub fn successors_among(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
    values: &BTreeSet<ModelString>,
) -> Vec<(BTreeMap<String, ParamValue>, TemperModelState)> {
    successors_budgeted(model, t, state, values, &Expansion::for_model(model))
}

pub(crate) fn successors_budgeted(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
    values: &BTreeSet<ModelString>,
    budget: &Expansion,
) -> Vec<(BTreeMap<String, ParamValue>, TemperModelState)> {
    successors_with(model, t, state, values, budget, |_| {})
}

/// [`successors`], adding to `cut` each counter and list a step it leaves
/// out would take past its bound, with that bound, and each count parameter
/// whose range was widened, with the largest value tried.
pub(crate) fn successors_noting_cuts(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
    cut: &mut BTreeMap<String, usize>,
) -> Vec<(BTreeMap<String, ParamValue>, TemperModelState)> {
    for (name, range) in count_ranges(model, t) {
        // A value past the range takes these counters past their bounds:
        // that step is left out, as any other step that goes past is.
        cut.extend(range.past);
        if range.widened {
            cut.insert(format!("params.{name}"), range.largest);
        }
    }
    let values = string_values(std::iter::once(state));
    successors_with(
        model,
        t,
        state,
        &values,
        &Expansion::for_model(model),
        |next| {
            cut.extend(outgrown(model, state, next));
        },
    )
}

/// [`successors_budgeted`], passing each step past the bounds to `left_out`.
fn successors_with(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
    values: &BTreeSet<ModelString>,
    budget: &Expansion,
    mut left_out: impl FnMut(&TemperModelState),
) -> Vec<(BTreeMap<String, ParamValue>, TemperModelState)> {
    let mut out = Vec::new();
    any_inputs(model, t, state, values, budget, |params| {
        if checks_inputs(model, t) && !inputs_allow(model, t, state, &params) {
            return false;
        }
        let mut next = state.clone();
        if let Some(to) = &t.to_state {
            next.status = to.clone();
        }
        apply_effects(&t.effects, &mut next, &t.name, &params);
        apply_string_params(&t.string_params, &mut next, &params);
        canonical_state(&mut next);
        if within_bounds(model, state, &next) {
            out.push((params, next));
        } else {
            left_out(&next);
        }
        false
    });
    out
}

/// The transition the runtime takes for action `name` in `status`: the first
/// with that name whose source states include `status` (temper-jit
/// `evaluate_ctx`). A later transition with the same name never fires from
/// `status`; when this one's guard fails, the action is refused.
pub fn rule_for<'a>(
    model: &'a TemperModel,
    name: &str,
    status: &str,
) -> Option<&'a ResolvedTransition> {
    model.transitions.iter().find(|t| {
        t.name == name && (t.from_states.is_empty() || t.from_states.iter().any(|s| s == status))
    })
}

/// `state` after transition `t` with `params`, with no renumbering of new
/// strings (the caller renumbers, over one entity or a joint state).
pub fn step(
    t: &ResolvedTransition,
    state: &TemperModelState,
    params: &BTreeMap<String, ParamValue>,
) -> TemperModelState {
    let mut next = state.clone();
    if let Some(to) = &t.to_state {
        next.status = to.clone();
    }
    apply_effects(&t.effects, &mut next, &t.name, params);
    apply_string_params(&t.string_params, &mut next, params);
    next
}

/// The transition with more parameter assignments than one step explores
/// ([`MAX_PARAM_ASSIGNMENTS`]), and how many it has. Stepping it would stop
/// at the assertion in `param_assignments`, so a caller asks this first.
pub fn oversized_transition(model: &TemperModel) -> Option<(&str, usize)> {
    model.transitions.iter().find_map(|t| {
        // Counted, not built: a parameter can write a counter with a large
        // bound, and building its candidates would allocate that many.
        let ranges = count_ranges(model, t);
        let assignments = t.params.iter().fold(1usize, |n, (name, kind)| {
            let choices = match kind {
                ParamKind::Count => ranges[name.as_str()].largest.saturating_add(1),
                ParamKind::Bool => 2,
                ParamKind::Str if t.string_params.iter().any(|p| p.name == *name) => 1,
                ParamKind::Str => string_literals(model, t, name).len() + 1,
            };
            n.saturating_mul(choices)
        });
        (assignments > MAX_PARAM_ASSIGNMENTS).then_some((t.name.as_str(), assignments))
    })
}

/// The values explored for one parameter of `t`.
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
fn string_literals(model: &TemperModel, t: &ResolvedTransition, name: &str) -> BTreeSet<String> {
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
pub(super) fn param_assignments(
    model: &TemperModel,
    t: &ResolvedTransition,
) -> Vec<BTreeMap<String, ParamValue>> {
    let ranges = count_ranges(model, t);
    let mut assignments = vec![BTreeMap::new()];
    for (name, kind) in &t.params {
        if *kind == ParamKind::Str && t.string_params.iter().any(|p| p.name == *name) {
            continue; // String products, including list inputs, use equality classes.
        }
        let candidates = param_candidates(model, t, name, kind, &ranges);
        assert!(
            assignments.len() * candidates.len() <= MAX_PARAM_ASSIGNMENTS,
            "transition '{}' has more than {MAX_PARAM_ASSIGNMENTS} parameter assignments",
            t.name
        );
        assignments = assignments
            .into_iter()
            .flat_map(|assignment| {
                candidates.iter().map(move |value| {
                    let mut next = assignment.clone();
                    next.insert(name.clone(), value.clone());
                    next
                })
            })
            .collect();
    }
    assignments
}

/// The largest value counter `var` is explored up to.
pub(crate) fn counter_bound(model: &TemperModel, var: &str) -> usize {
    model
        .counter_bounds
        .get(var)
        .copied()
        .unwrap_or(model.default_max_counter)
}

/// The longest list `var` is explored up to.
pub(crate) fn list_bound(model: &TemperModel, var: &str) -> usize {
    model
        .list_bounds
        .get(var)
        .copied()
        .unwrap_or(model.default_max_counter)
}

/// Whether `next` stays within the exploration bounds, or at least grew no
/// counter or list beyond them.
pub(crate) fn within_bounds(
    model: &TemperModel,
    before: &TemperModelState,
    next: &TemperModelState,
) -> bool {
    let counters_ok = next.counters.iter().all(|(var, value)| {
        *value <= counter_bound(model, var)
            || *value <= before.counters.get(var).copied().unwrap_or(0)
    });
    let lists_ok = next.lists.iter().all(|(var, items)| {
        items.len() <= list_bound(model, var)
            || items.len() <= before.lists.get(var).map_or(0, Vec::len)
    });
    counters_ok && lists_ok
}

/// The counters and lists `next` holds past the bound they are explored to
/// that grew from `before`, each with that bound: why [`within_bounds`]
/// leaves a step out.
fn outgrown<'a>(
    model: &'a TemperModel,
    before: &'a TemperModelState,
    next: &'a TemperModelState,
) -> impl Iterator<Item = (String, usize)> + 'a {
    let counters = next.counters.iter().filter_map(move |(var, value)| {
        let bound = counter_bound(model, var);
        let grew = *value > before.counters.get(var).copied().unwrap_or(0);
        (*value > bound && grew).then(|| (var.clone(), bound))
    });
    let lists = next.lists.iter().filter_map(move |(var, items)| {
        let bound = list_bound(model, var);
        let grew = items.len() > before.lists.get(var).map_or(0, Vec::len);
        (items.len() > bound && grew).then(|| (var.clone(), bound))
    });
    counters.chain(lists)
}

/// Apply model effects to the provided state, reading `params.p` from
/// `params`. Mirrors the runtime: `-=` stops at 0, `remove_at` out of
/// range is a no-op and `remove` of an absent value is a no-op.
///
/// `action_name` is used to generate deterministic fresh list elements.
/// Third interpreter. Production apply is `temper-server` `entity_actor/effects.rs`.
pub fn apply_effects(
    effects: &[ModelEffect],
    state: &mut TemperModelState,
    action_name: &str,
    params: &BTreeMap<String, ParamValue>,
) {
    let count = |arg: &Arg, state: &TemperModelState| -> usize {
        match arg {
            Arg::Lit(Literal::Int(n)) => usize::try_from(*n).unwrap_or(0),
            Arg::Var(name) => state.counters.get(name).copied().unwrap_or(0),
            Arg::Param(name) => match params.get(name) {
                Some(ParamValue::Count(n)) => *n,
                _ => 0,
            },
            Arg::Lit(_) => 0,
        }
    };
    for effect in effects {
        match effect {
            ModelEffect::SetCounter { var, value } => {
                let value = count(value, state);
                state.counters.insert(var.clone(), value);
            }
            ModelEffect::AddCounter { var, value } => {
                let delta = count(value, state);
                *state.counters.entry(var.clone()).or_insert(0) += delta;
            }
            ModelEffect::SubCounter { var, value } => {
                let delta = count(value, state);
                let entry = state.counters.entry(var.clone()).or_insert(0);
                *entry = entry.saturating_sub(delta);
            }
            ModelEffect::SetBool { var, value } => {
                let value = match value {
                    Arg::Lit(Literal::Bool(b)) => *b,
                    Arg::Var(name) => state.booleans.get(name).copied().unwrap_or(false),
                    Arg::Param(name) => matches!(params.get(name), Some(ParamValue::Bool(true))),
                    Arg::Lit(_) => false,
                };
                state.booleans.insert(var.clone(), value);
            }
            ModelEffect::SetString { var, value } => {
                if let Some(current) = state.strings.get_mut(var) {
                    *current = ModelString::from(value.clone());
                }
            }
            ModelEffect::ListAppend { var, value } => {
                let placeholder = || {
                    format!(
                        "{action_name}#{}",
                        state.lists.get(var).map_or(1, |v| v.len() + 1)
                    )
                    .into()
                };
                let element = match value {
                    Arg::Lit(Literal::Str(text)) => text.clone().into(),
                    Arg::Param(name) => match params.get(name) {
                        Some(ParamValue::Str(value)) => value.clone(),
                        _ => placeholder(),
                    },
                    _ => placeholder(),
                };
                state.lists.entry(var.clone()).or_default().push(element);
            }
            ModelEffect::ListRemove { var, value } => {
                let element = match value {
                    Arg::Lit(Literal::Str(text)) => Some(text.clone().into()),
                    Arg::Param(name) => match params.get(name) {
                        Some(ParamValue::Str(value)) => Some(value.clone()),
                        _ => None,
                    },
                    _ => None,
                };
                if let (Some(element), Some(entry)) = (element, state.lists.get_mut(var)) {
                    entry.retain(|item| *item != element);
                }
            }
            ModelEffect::ListRemoveAt { var, index } => {
                let index = count(index, state);
                if let Some(entry) = state.lists.get_mut(var)
                    && index < entry.len()
                {
                    entry.remove(index);
                }
            }
        }
    }
}

/// Collect every `(list_var, value)` pair tested by `'value' in list_var`.
pub fn collect_list_contains_pairs(expr: &Expr, pairs: &mut BTreeSet<(String, String)>) {
    match expr {
        Expr::In {
            value: Operand::Lit(temper_spec::predicate::Literal::Str(value)),
            set: Set::Var(var),
            ..
        } => {
            pairs.insert((var.clone(), value.clone()));
        }
        Expr::Not(inner) => collect_list_contains_pairs(inner, pairs),
        Expr::And(parts) | Expr::Or(parts) => {
            for part in parts {
                collect_list_contains_pairs(part, pairs);
            }
        }
        Expr::Implies(a, b) => {
            collect_list_contains_pairs(a, pairs);
            collect_list_contains_pairs(b, pairs);
        }
        _ => {}
    }
}

#[cfg(test)]
#[path = "semantics_test.rs"]
mod tests;
