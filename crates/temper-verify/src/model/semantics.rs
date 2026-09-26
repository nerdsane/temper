//! Shared concrete guard/effect semantics for verification backends.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::predicate::{
    Arg, Env, Expr, Literal, Operand, ParamKind, Set, Truth, Val, VarKind, eval,
};

use super::types::{ModelEffect, ParamValue, ResolvedTransition, TemperModel, TemperModelState};

/// Parameter assignments explored per step for one transition, at most
/// (TigerStyle budget: `successors` materializes every assignment).
pub const MAX_PARAM_ASSIGNMENTS: usize = 4096;

/// A model state as a predicate environment. Status, counters, booleans and
/// lists are modeled; declared counters, booleans and lists missing from the
/// state read as their zero value. String and numeric variables, entity
/// fields and related-entity statuses are not modeled and read as unknown.
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
            Some(VarKind::List) => self
                .state
                .lists
                .get(name)
                .map_or(Val::Strs(&[]), |items| Val::Strs(items)),
            Some(VarKind::Str | VarKind::Num) | None => Val::Unknown,
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
/// counts `0..=bound`, both booleans, and for list elements every literal a
/// guard or invariant compares that list against plus one fresh string.
/// Assignments that grow a counter past its bound (or a list past the
/// default bound) are pruned, as in bounded exploration.
pub fn successors(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
) -> Vec<(BTreeMap<String, ParamValue>, TemperModelState)> {
    let mut out = Vec::new();
    for params in param_assignments(model, t) {
        let mut next = state.clone();
        if let Some(to) = &t.to_state {
            next.status = to.clone();
        }
        apply_effects(&t.effects, &mut next, &t.name, &params);
        if within_bounds(model, state, &next) {
            out.push((params, next));
        }
    }
    out
}

/// The cartesian product of each parameter's candidate values.
fn param_assignments(
    model: &TemperModel,
    t: &ResolvedTransition,
) -> Vec<BTreeMap<String, ParamValue>> {
    let count_bound = model
        .counter_bounds
        .values()
        .copied()
        .max()
        .unwrap_or(0)
        .max(model.default_max_counter);
    let mut assignments = vec![BTreeMap::new()];
    for (name, kind) in &t.params {
        let candidates: Vec<ParamValue> = match kind {
            ParamKind::Count => (0..=count_bound).map(ParamValue::Count).collect(),
            ParamKind::Bool => vec![ParamValue::Bool(false), ParamValue::Bool(true)],
            ParamKind::Str => {
                let mut values: BTreeSet<String> = BTreeSet::new();
                for effect in &t.effects {
                    if let ModelEffect::ListAppend {
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
                    .into_iter()
                    .map(ParamValue::Str)
                    .chain(std::iter::once(ParamValue::Fresh))
                    .collect()
            }
        };
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

/// Whether `next` stays within the exploration bounds, or at least grew no
/// counter or list beyond them.
fn within_bounds(model: &TemperModel, before: &TemperModelState, next: &TemperModelState) -> bool {
    let counters_ok = next.counters.iter().all(|(var, value)| {
        let bound = model
            .counter_bounds
            .get(var)
            .copied()
            .unwrap_or(model.default_max_counter);
        *value <= bound || *value <= before.counters.get(var).copied().unwrap_or(0)
    });
    let lists_ok = next.lists.iter().all(|(var, items)| {
        items.len() <= model.default_max_counter
            || items.len() <= before.lists.get(var).map_or(0, Vec::len)
    });
    counters_ok && lists_ok
}

/// Apply model effects to the provided state, reading `params.p` from
/// `params`. Mirrors the runtime: `-=` stops at 0 and `remove_at` out of
/// range is a no-op.
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
            ModelEffect::ListAppend { var, value } => {
                let entry = state.lists.entry(var.clone()).or_default();
                let element = match value {
                    Arg::Lit(Literal::Str(text)) => text.clone(),
                    Arg::Param(name) => match params.get(name) {
                        Some(ParamValue::Str(text)) => text.clone(),
                        _ => format!("{action_name}#{}", entry.len() + 1),
                    },
                    _ => format!("{action_name}#{}", entry.len() + 1),
                };
                entry.push(element);
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
mod tests {
    use super::*;

    fn kinds() -> BTreeMap<String, VarKind> {
        [
            ("items", VarKind::Counter),
            ("ready", VarKind::Bool),
            ("tags", VarKind::List),
            ("goal", VarKind::Str),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }

    fn state(status: &str) -> TemperModelState {
        TemperModelState {
            status: status.to_string(),
            counters: BTreeMap::new(),
            booleans: BTreeMap::new(),
            lists: BTreeMap::new(),
        }
    }

    fn g(source: &str) -> Expr {
        temper_spec::predicate::parse(source).unwrap()
    }

    #[test]
    fn declared_variables_missing_from_state_read_as_zero_values() {
        let s = state("A");
        let k = kinds();
        assert!(evaluate_guard(&g("items == 0 && items < 3"), &k, &s));
        assert!(evaluate_guard(&g("!ready && len(tags) == 0"), &k, &s));
        assert!(!evaluate_guard(&g("items >= 2"), &k, &s));
    }

    #[test]
    fn evaluates_modeled_state() {
        let mut s = state("Draft");
        s.counters.insert("items".into(), 2);
        s.booleans.insert("ready".into(), true);
        s.lists.insert("tags".into(), vec!["vip".into()]);
        let k = kinds();
        assert!(evaluate_guard(
            &g("status == 'Draft' && items >= 2 && ready && 'vip' in tags"),
            &k,
            &s
        ));
        assert!(!evaluate_guard(&g("status in ['Closed']"), &k, &s));
    }

    #[test]
    fn unmodeled_values_are_free_for_exploration_and_absent_for_enablement() {
        let s = state("A");
        let k = kinds();
        for unknown in [
            "P[p].status == 'Done'",
            "goal != ''",
            "!(P[p].status == 'Done')",
        ] {
            let guard = g(unknown);
            assert!(guard_may_hold(&guard, &k, &s), "{unknown} may hold");
            assert!(
                !evaluate_guard(&guard, &k, &s),
                "{unknown} is not guaranteed"
            );
        }
        // A local condition that is false decides, whatever the unknown is.
        let guard = g("items > 5 && P[p].status == 'Done'");
        assert!(!guard_may_hold(&guard, &k, &s));
        // A local condition that is true decides a disjunction.
        let guard = g("items == 0 || P[p].status == 'Done'");
        assert!(evaluate_guard(&guard, &k, &s));
    }

    #[test]
    fn collects_list_membership_atoms_through_any_connective() {
        let mut pairs = BTreeSet::new();
        collect_list_contains_pairs(&g("!('a' in tags) || ready => 'b' in tags"), &mut pairs);
        assert_eq!(
            pairs.into_iter().collect::<Vec<_>>(),
            vec![("tags".into(), "a".into()), ("tags".into(), "b".into())]
        );
    }
}
