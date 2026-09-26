//! Shared concrete guard/effect semantics for verification backends.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::predicate::{Env, Expr, Operand, Set, Truth, Val, VarKind, eval};

use super::types::{ModelEffect, TemperModelState};

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
            Some(VarKind::Bool) => Val::Bool(self.state.booleans.get(name).copied().unwrap_or(false)),
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
pub fn evaluate_guard(guard: &Expr, kinds: &BTreeMap<String, VarKind>, state: &TemperModelState) -> bool {
    truth(guard, kinds, state).must_hold()
}

/// Whether a guard **may** hold, for state-space exploration.
///
/// Unknown values are free (nondeterministic) booleans (ADR-0149): the edge
/// is offered when the guard could hold (guard-true branch); the BFS covers the
/// guard-false branch by also exploring states where the edge is not taken.
/// Locally resolvable parts still decide: an unsatisfiable local condition
/// keeps the guard false whatever the unknowns are.
pub fn guard_may_hold(guard: &Expr, kinds: &BTreeMap<String, VarKind>, state: &TemperModelState) -> bool {
    truth(guard, kinds, state).may_hold()
}

/// Apply model effects to the provided state.
///
/// `action_name` is used to generate deterministic symbolic list elements.
/// Third interpreter. Production apply is `temper-server` `entity_actor/effects.rs`.
pub fn apply_effects(effects: &[ModelEffect], state: &mut TemperModelState, action_name: &str) {
    for effect in effects {
        match effect {
            ModelEffect::IncrementCounter(var) => {
                let entry = state.counters.entry(var.clone()).or_insert(0);
                *entry += 1;
            }
            ModelEffect::DecrementCounter(var) => {
                let entry = state.counters.entry(var.clone()).or_insert(0);
                *entry = entry.saturating_sub(1);
            }
            ModelEffect::SetBool { var, value } => {
                state.booleans.insert(var.clone(), *value);
            }
            ModelEffect::ListAppend(var) => {
                let entry = state.lists.entry(var.clone()).or_default();
                let next_idx = entry.len() + 1;
                entry.push(format!("{action_name}#{next_idx}"));
            }
            ModelEffect::ListRemoveAt(var) => {
                if let Some(entry) = state.lists.get_mut(var)
                    && !entry.is_empty()
                {
                    entry.remove(0);
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
        for unknown in ["P[p].status == 'Done'", "goal != ''", "!(P[p].status == 'Done')"] {
            let guard = g(unknown);
            assert!(guard_may_hold(&guard, &k, &s), "{unknown} may hold");
            assert!(!evaluate_guard(&guard, &k, &s), "{unknown} is not guaranteed");
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
