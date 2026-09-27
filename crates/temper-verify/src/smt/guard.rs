//! Independent guard satisfiability queries.

use super::{
    Bool, Int, ListSymbolicVars, SatResult, Solver, Symbols, TemperModel, encode,
    encode_state_membership, make_status_var,
};
use std::collections::{BTreeMap, BTreeSet};

use crate::model::semantics::collect_list_contains_pairs;

/// Check every transition's guard against the same bounded state space.
pub(super) fn check_guard_satisfiability(
    model: &TemperModel,
    max_counter: usize,
) -> Vec<(String, bool)> {
    model
        .transitions
        .iter()
        .map(|transition| {
            let solver = Solver::new();

            // Invalid from-states are rejected before issuing a SAT query.
            if !transition.from_states.is_empty()
                && !transition
                    .from_states
                    .iter()
                    .any(|state| model.states.contains(state))
            {
                return (transition.name.clone(), false);
            }

            let counter_vars = make_counter_vars(model, &solver, max_counter);
            let bool_vars = make_bool_vars(model);
            let list_vars = make_list_vars(model, &solver, max_counter);
            let status_var = make_status_var(model, &solver);

            if !transition.from_states.is_empty() {
                solver.assert(encode_state_membership(
                    &status_var,
                    &transition.from_states,
                    model,
                ));
            }
            let symbols = Symbols {
                counters: counter_vars.into_iter().collect(),
                bools: bool_vars.into_iter().collect(),
                lists: &list_vars,
                status: status_var,
                prefix: "",
            };
            solver.assert(encode(&transition.guard, &symbols, model));

            let sat = matches!(solver.check(), SatResult::Sat);
            (transition.name.clone(), sat)
        })
        .collect()
}

/// Create Z3 integer variables for each counter, bounded [0, max_counter].
fn make_counter_vars(
    model: &TemperModel,
    solver: &Solver,
    max_counter: usize,
) -> Vec<(String, Int)> {
    let zero = Int::from_i64(0);
    let max_val = Int::from_i64(max_counter as i64);

    model
        .initial_counters
        .keys()
        .map(|name| {
            let var = Int::new_const(name.as_str());
            solver.assert(var.ge(&zero));
            solver.assert(var.le(&max_val));
            (name.clone(), var)
        })
        .collect()
}

/// Create Z3 boolean variables for each boolean state var.
fn make_bool_vars(model: &TemperModel) -> Vec<(String, Bool)> {
    model
        .initial_booleans
        .keys()
        .map(|name| {
            let var = Bool::new_const(name.as_str());
            (name.clone(), var)
        })
        .collect()
}

/// Create exact bounded symbolic list variables:
/// - `len` in `[0, max_counter]`
/// - `elem_0..elem_{max_counter-1}` for position values
fn make_list_vars(model: &TemperModel, solver: &Solver, max_counter: usize) -> ListSymbolicVars {
    let zero = Int::from_i64(0);
    let max_val = Int::from_i64(max_counter as i64);
    let mut len_vars = BTreeMap::new();
    let mut elem_vars = BTreeMap::new();

    for name in model.initial_lists.keys() {
        let len_var = Int::new_const(format!("{name}_len"));
        solver.assert(len_var.ge(&zero));
        solver.assert(len_var.le(&max_val));
        len_vars.insert(name.clone(), len_var);

        let elements = (0..max_counter)
            .map(|idx| Int::new_const(format!("{name}_elem_{idx}")))
            .collect::<Vec<_>>();
        elem_vars.insert(name.clone(), elements);
    }

    let mut values = BTreeSet::new();
    for transition in &model.transitions {
        let mut pairs = BTreeSet::new();
        collect_list_contains_pairs(&transition.guard, &mut pairs);
        for (_, value) in pairs {
            values.insert(value);
        }
    }
    for list in model.initial_lists.values() {
        for value in list {
            values.insert(value.clone());
        }
    }

    let value_atoms = values
        .into_iter()
        .enumerate()
        .map(|(idx, value)| (value, idx as i64))
        .collect::<BTreeMap<_, _>>();

    ListSymbolicVars {
        len_vars,
        elem_vars,
        value_atoms,
    }
}

#[cfg(test)]
mod tests;
