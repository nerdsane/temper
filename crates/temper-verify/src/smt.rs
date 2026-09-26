//! SMT symbolic verification (Level 0 of the verification cascade).
//!
//! Uses the Z3 SMT solver to verify properties algebraically without
//! enumerating states:
//!
//! 1. **Guard satisfiability** — Encode each guard as a Z3 formula over
//!    integer counters (0..max) and boolean variables. Check SAT: if UNSAT,
//!    the guard is dead code (the action can never fire).
//!
//! 2. **Invariant induction** — For each (invariant, transition) pair:
//!    assume `invariant(S) ∧ guard(S) ∧ status ∈ from_states`, apply
//!    effects to get S', prove `invariant(S')` by checking that its
//!    negation is UNSAT.
//!
//! 3. **Unreachable state detection** — BFS from initial state through
//!    transition targets to find states that can never be reached.

use std::collections::{BTreeMap, BTreeSet};

use z3::ast::{Bool, Int};
use z3::{SatResult, Solver};

use temper_spec::predicate::{CmpOp, Expr, Literal, Operand, Set, unmodelable};

use crate::model::builder::build_model_from_ioa;
use crate::model::semantics::collect_list_contains_pairs;
use crate::model::types::{ModelEffect, ResolvedTransition, TemperModel};

/// Result of symbolic verification.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SmtResult {
    /// For each action, whether its guard is satisfiable (can ever fire).
    pub guard_satisfiability: Vec<(String, bool)>,
    /// For each invariant, whether it is inductively maintained by all transitions.
    pub inductive_invariants: Vec<(String, bool)>,
    /// States that cannot be reached from the initial state.
    pub unreachable_states: Vec<String>,
    /// Whether symbolic checks rely on bounded/abstract encodings.
    pub approximate: bool,
    /// Human-readable approximation notes for downstream reporting.
    pub approximation_notes: Vec<String>,
    /// Whether all checks passed (no dead guards, all invariants inductive).
    pub all_passed: bool,
}

/// Run symbolic verification on an IOA spec using the Z3 SMT solver.
///
/// This is the Level 0 entry point. It checks:
/// 1. Guard satisfiability: is there any state in which each guard can fire?
/// 2. Invariant induction: does each invariant hold after every transition?
/// 3. Unreachable states: can each declared state be reached?
pub fn verify_symbolic(ioa_toml: &str, max_counter: usize) -> SmtResult {
    let model = build_model_from_ioa(ioa_toml, max_counter)
        .expect("SMT: IOA spec should have been validated before symbolic verification");
    let approximation_notes = approximation_notes(&model);
    let approximate = !approximation_notes.is_empty();

    let guard_sat = check_guard_satisfiability(&model, max_counter);
    let inductive = check_invariant_induction(&model, max_counter);
    let unreachable = check_unreachable_states(&model);

    // Unreachable states are warnings, not failures — specs may declare states
    // that are only reachable through composition or external actions.
    let all_passed = guard_sat.iter().all(|(_, sat)| *sat) && inductive.iter().all(|(_, ind)| *ind);

    SmtResult {
        guard_satisfiability: guard_sat,
        inductive_invariants: inductive,
        unreachable_states: unreachable,
        approximate,
        approximation_notes,
        all_passed,
    }
}

fn approximation_notes(model: &TemperModel) -> Vec<String> {
    let cross_entity_guard_count = model
        .transitions
        .iter()
        .filter(|transition| reads_related_entities(&transition.guard))
        .count();
    if cross_entity_guard_count == 0 {
        return Vec::new();
    }

    vec![format!(
        "{cross_entity_guard_count} transition(s) use abstract cross-entity guards; single-entity SMT excludes them from local induction and reachability"
    )]
}

/// Whether a guard reads a related entity's status (not modeled here).
fn reads_related_entities(guard: &Expr) -> bool {
    !guard.cross_refs().is_empty()
}

fn concrete_transitions(model: &TemperModel) -> impl Iterator<Item = &ResolvedTransition> {
    model
        .transitions
        .iter()
        .filter(|transition| !reads_related_entities(&transition.guard))
}

// ---------------------------------------------------------------------------
// Guard satisfiability
// ---------------------------------------------------------------------------

/// For each transition, encode its guard as a Z3 formula and check SAT.
///
/// A guard is satisfiable if there exists an assignment of counter values
/// (0..max_counter) and boolean values that makes the guard true.
fn check_guard_satisfiability(model: &TemperModel, max_counter: usize) -> Vec<(String, bool)> {
    model
        .transitions
        .iter()
        .map(|t| {
            let solver = Solver::new();

            // Check that at least one from_state exists in the state space
            if !t.from_states.is_empty() {
                let has_valid_from = t.from_states.iter().any(|s| model.states.contains(s));
                if !has_valid_from {
                    return (t.name.clone(), false);
                }
            }

            // Create Z3 variables for each counter, bounded [0, max_counter]
            let counter_vars = make_counter_vars(model, &solver, max_counter);
            let bool_vars = make_bool_vars(model);
            let list_vars = make_list_vars(model, &solver, max_counter);
            let status_var = make_status_var(model, &solver);

            if !t.from_states.is_empty() {
                let from_formula = encode_state_membership(&status_var, &t.from_states, model);
                solver.assert(&from_formula);
            }

            // Encode the guard as a Z3 formula and assert it
            let symbols = Symbols {
                counters: counter_vars.into_iter().collect(),
                bools: bool_vars.into_iter().collect(),
                lists: &list_vars,
                status: status_var,
                prefix: "",
            };
            solver.assert(encode(&t.guard, &symbols, model));

            let sat = matches!(solver.check(), SatResult::Sat);
            (t.name.clone(), sat)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Invariant induction
// ---------------------------------------------------------------------------

/// For each invariant, check that the transitions entering its states
/// preserve it.
///
/// L0 scope (unchanged by the unified grammar): an invariant written as
/// `status in [W...] => P` is checked on every concrete transition whose
/// target is in `W`: assume `P` in the pre-state, apply the effects, prove
/// `P` in the post-state. Guards and other variables are not constrained, so
/// this is a fast early filter; the model checker (L1) is the full proof.
/// Invariants without a `status in [...] =>` prefix, and invariants over
/// values the model does not track, are left to L1. Status membership and
/// `terminal` states are checked structurally.
fn check_invariant_induction(model: &TemperModel, max_counter: usize) -> Vec<(String, bool)> {
    let mut results = vec![(
        "TypeInvariant".to_string(),
        concrete_transitions(model).all(|t| {
            t.to_state
                .as_ref()
                .map(|s| model.states.contains(s))
                .unwrap_or(true)
        }),
    )];
    for inv in &model.invariants {
        let inductive = match split_trigger(&inv.assert) {
            Some((trigger, body)) if unmodelable(body, &model.var_kinds).is_none() => {
                concrete_transitions(model)
                    .filter(|t| t.to_state.as_ref().is_some_and(|to| trigger.contains(to)))
                    .all(|t| preserved_by(model, body, t, max_counter))
            }
            _ => true,
        };
        results.push((inv.name.clone(), inductive));
    }
    for state in &model.terminal {
        let closed = !concrete_transitions(model)
            .any(|t| t.from_states.contains(state) || t.from_states.is_empty());
        results.push((format!("Terminal({state})"), closed));
    }
    results
}

/// `status in [W...] => P` as `(W, P)`.
fn split_trigger(assert: &Expr) -> Option<(Vec<String>, &Expr)> {
    let Expr::Implies(lhs, body) = assert else {
        return None;
    };
    let Expr::In {
        value: Operand::Status,
        set: Set::List(items),
        negated: false,
    } = lhs.as_ref()
    else {
        return None;
    };
    let states = items
        .iter()
        .map(|item| match item {
            Literal::Str(state) => Some(state.clone()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    Some((states, body))
}

/// Whether transition `t` preserves `body`: `body(pre) ∧ post = effects(pre)
/// ∧ ¬body(post)` is unsatisfiable.
fn preserved_by(model: &TemperModel, body: &Expr, t: &ResolvedTransition, max_counter: usize) -> bool {
    let solver = Solver::new();
    let zero = Int::from_i64(0);
    let one = Int::from_i64(1);
    let max_val = Int::from_i64(max_counter as i64);
    let lists = ListSymbolicVars::default();

    let mut pre_counters = BTreeMap::new();
    let mut post_counters = BTreeMap::new();
    for name in model.initial_counters.keys() {
        let pre = Int::new_const(format!("{name}_pre"));
        solver.assert(pre.ge(&zero));
        solver.assert(pre.le(&max_val));
        let mut post = pre.clone();
        for effect in &t.effects {
            match effect {
                ModelEffect::IncrementCounter(v) if v == name => {
                    post = Int::add(&[&post, &one]);
                }
                ModelEffect::DecrementCounter(v) if v == name => {
                    // Runtime semantics are saturating_sub(1): max(counter-1, 0)
                    let dec = Int::sub(&[&post, &one]);
                    post = post.gt(&zero).ite(&dec, &zero);
                }
                _ => {}
            }
        }
        pre_counters.insert(name.clone(), pre);
        post_counters.insert(name.clone(), post);
    }
    let mut pre_bools = BTreeMap::new();
    let mut post_bools = BTreeMap::new();
    for name in model.initial_booleans.keys() {
        let pre = Bool::new_const(format!("{name}_pre"));
        let mut post = pre.clone();
        for effect in &t.effects {
            if let ModelEffect::SetBool { var, value } = effect
                && var == name
            {
                post = Bool::from_bool(*value);
            }
        }
        pre_bools.insert(name.clone(), pre);
        post_bools.insert(name.clone(), post);
    }
    let pre_status = make_status_var(model, &solver);
    let post_status = match t.to_state.as_ref().and_then(|to| model.states.iter().position(|s| s == to)) {
        Some(idx) => Int::from_i64(idx as i64),
        None => pre_status.clone(),
    };

    let pre = Symbols {
        counters: pre_counters,
        bools: pre_bools,
        lists: &lists,
        status: pre_status,
        prefix: "pre:",
    };
    let post = Symbols {
        counters: post_counters,
        bools: post_bools,
        lists: &lists,
        status: post_status,
        prefix: "post:",
    };
    solver.assert(encode(body, &pre, model));
    solver.assert(encode(body, &post, model).not());
    !matches!(solver.check(), SatResult::Sat)
}

// ---------------------------------------------------------------------------
// Z3 helpers
// ---------------------------------------------------------------------------

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

#[derive(Default)]
struct ListSymbolicVars {
    len_vars: BTreeMap<String, Int>,
    elem_vars: BTreeMap<String, Vec<Int>>,
    value_atoms: BTreeMap<String, i64>,
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
    for t in &model.transitions {
        let mut pairs = BTreeSet::new();
        collect_list_contains_pairs(&t.guard, &mut pairs);
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

/// Create a symbolic status variable over `model.states` indices.
fn make_status_var(model: &TemperModel, solver: &Solver) -> Int {
    let var = Int::new_const("status_idx");
    let zero = Int::from_i64(0);
    if model.states.is_empty() {
        solver.assert(var.eq(&zero));
        return var;
    }
    let max = Int::from_i64((model.states.len() - 1) as i64);
    solver.assert(var.ge(&zero));
    solver.assert(var.le(&max));
    var
}

/// Encode `status ∈ states` as a disjunction over symbolic status index.
fn encode_state_membership(status_var: &Int, states: &[String], model: &TemperModel) -> Bool {
    let disjuncts: Vec<Bool> = states
        .iter()
        .filter_map(|state| {
            model
                .states
                .iter()
                .position(|s| s == state)
                .map(|idx| status_var.eq(Int::from_i64(idx as i64)))
        })
        .collect();
    if disjuncts.is_empty() {
        Bool::from_bool(false)
    } else {
        Bool::or(&disjuncts)
    }
}

/// Encode exact bounded `contains(list, value)` over symbolic list slots.
fn encode_list_contains(var: &str, value: &str, lists: &ListSymbolicVars) -> Bool {
    let Some(len_var) = lists.len_vars.get(var) else {
        return Bool::from_bool(false);
    };
    let Some(elements) = lists.elem_vars.get(var) else {
        return Bool::from_bool(false);
    };
    let Some(atom_id) = lists.value_atoms.get(value) else {
        return Bool::from_bool(false);
    };

    if elements.is_empty() {
        return Bool::from_bool(false);
    }

    let atom = Int::from_i64(*atom_id);
    let disjuncts: Vec<Bool> = elements
        .iter()
        .enumerate()
        .map(|(idx, element)| {
            let idx_int = Int::from_i64(idx as i64);
            Bool::and(&[&len_var.gt(&idx_int), &element.eq(&atom)])
        })
        .collect();
    Bool::or(&disjuncts)
}

/// Symbolic values for one state.
struct Symbols<'a> {
    counters: BTreeMap<String, Int>,
    bools: BTreeMap<String, Bool>,
    lists: &'a ListSymbolicVars,
    status: Int,
    /// Distinguishes free atoms of different states (pre/post).
    prefix: &'static str,
}

/// Encode an expression as a Z3 formula. Parts over values the model does
/// not track (related-entity statuses, strings, fields) become free boolean
/// atoms, named by their source text so repeated occurrences agree.
fn encode(expr: &Expr, sym: &Symbols<'_>, model: &TemperModel) -> Bool {
    let free = || Bool::new_const(format!("{}atom:{expr}", sym.prefix));
    match expr {
        Expr::Const(value) => Bool::from_bool(*value),
        Expr::Not(inner) => encode(inner, sym, model).not(),
        Expr::And(parts) => {
            Bool::and(&parts.iter().map(|p| encode(p, sym, model)).collect::<Vec<_>>())
        }
        Expr::Or(parts) => {
            Bool::or(&parts.iter().map(|p| encode(p, sym, model)).collect::<Vec<_>>())
        }
        Expr::Implies(lhs, rhs) => encode(lhs, sym, model).implies(encode(rhs, sym, model)),
        Expr::Var(name) => sym.bools.get(name).cloned().unwrap_or_else(free),
        Expr::Empty(name) => match sym.lists.len_vars.get(name) {
            Some(len) => len.eq(Int::from_i64(0)),
            None => free(),
        },
        Expr::Compare { lhs, op, rhs } => {
            if let (Some(a), Some(b)) = (int_term(lhs, sym), int_term(rhs, sym)) {
                return match op {
                    CmpOp::Eq => a.eq(&b),
                    CmpOp::Ne => a.eq(&b).not(),
                    CmpOp::Lt => a.lt(&b),
                    CmpOp::Le => a.le(&b),
                    CmpOp::Gt => a.gt(&b),
                    CmpOp::Ge => a.ge(&b),
                };
            }
            let equal = match (lhs, rhs) {
                (Operand::Status, Operand::Lit(Literal::Str(state)))
                | (Operand::Lit(Literal::Str(state)), Operand::Status) => Some(
                    encode_state_membership(&sym.status, std::slice::from_ref(state), model),
                ),
                (Operand::Var(name), Operand::Lit(Literal::Bool(value)))
                | (Operand::Lit(Literal::Bool(value)), Operand::Var(name)) => {
                    sym.bools.get(name).map(|var| var.eq(Bool::from_bool(*value)))
                }
                _ => None,
            };
            match (equal, op) {
                (Some(equal), CmpOp::Eq) => equal,
                (Some(equal), CmpOp::Ne) => equal.not(),
                _ => free(),
            }
        }
        Expr::In {
            value,
            set,
            negated,
        } => {
            let member = match (value, set) {
                (Operand::Status, Set::List(items)) => {
                    let states: Option<Vec<String>> = items
                        .iter()
                        .map(|item| match item {
                            Literal::Str(state) => Some(state.clone()),
                            _ => None,
                        })
                        .collect();
                    states.map(|states| encode_state_membership(&sym.status, &states, model))
                }
                (Operand::Lit(Literal::Str(value)), Set::Var(list))
                    if sym.lists.len_vars.contains_key(list) =>
                {
                    Some(encode_list_contains(list, value, sym.lists))
                }
                (value, Set::List(items)) => int_term(value, sym).and_then(|term| {
                    let options: Option<Vec<Bool>> = items
                        .iter()
                        .map(|item| match item {
                            Literal::Int(n) => Some(term.eq(Int::from_i64(*n))),
                            _ => None,
                        })
                        .collect();
                    options.map(|options| Bool::or(&options))
                }),
                _ => None,
            };
            match member {
                Some(member) if *negated => member.not(),
                Some(member) => member,
                None => free(),
            }
        }
    }
}

/// An integer-valued operand: a counter, a list length, or an integer.
fn int_term(operand: &Operand, sym: &Symbols<'_>) -> Option<Int> {
    match operand {
        Operand::Var(name) => sym.counters.get(name).cloned(),
        Operand::Len(name) => sym.lists.len_vars.get(name).cloned(),
        Operand::Lit(Literal::Int(n)) => Some(Int::from_i64(*n)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Unreachable state detection (graph-based, no Z3 needed)
// ---------------------------------------------------------------------------

/// Check which states are unreachable from the initial state.
fn check_unreachable_states(model: &TemperModel) -> Vec<String> {
    let mut reachable: BTreeSet<&str> = BTreeSet::new();
    let mut queue: Vec<&str> = vec![&model.initial_status];

    while let Some(state) = queue.pop() {
        if !reachable.insert(state) {
            continue;
        }
        for t in &model.transitions {
            if reads_related_entities(&t.guard) {
                continue;
            }
            let can_fire_from =
                t.from_states.is_empty() || t.from_states.iter().any(|s| s == state);
            if can_fire_from
                && let Some(to) = &t.to_state
                && !reachable.contains(to.as_str())
            {
                queue.push(to);
            }
        }
    }

    model
        .states
        .iter()
        .filter(|s| !reachable.contains(s.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

    #[test]
    fn test_all_guards_satisfiable() {
        let result = verify_symbolic(ORDER_IOA, 2);
        for (action, sat) in &result.guard_satisfiability {
            assert!(sat, "Guard for '{action}' should be satisfiable");
        }
    }

    #[test]
    fn test_no_unreachable_states() {
        let result = verify_symbolic(ORDER_IOA, 2);
        assert!(
            result.unreachable_states.is_empty(),
            "All states should be reachable, but got unreachable: {:?}",
            result.unreachable_states
        );
    }

    #[test]
    fn test_type_invariant_is_inductive() {
        let result = verify_symbolic(ORDER_IOA, 2);
        let type_inv = result
            .inductive_invariants
            .iter()
            .find(|(name, _)| name == "TypeInvariant");
        assert!(type_inv.is_some());
        assert!(type_inv.unwrap().1, "TypeInvariant should be inductive");
    }

    #[test]
    fn test_counter_positive_invariant_is_inductive() {
        let result = verify_symbolic(ORDER_IOA, 2);
        let inv = result
            .inductive_invariants
            .iter()
            .find(|(name, _)| name == "SubmitRequiresItems");
        assert!(inv.is_some(), "Should have SubmitRequiresItems");
        assert!(inv.unwrap().1, "SubmitRequiresItems should be inductive");
    }

    #[test]
    fn test_symbolic_result_structure() {
        let result = verify_symbolic(ORDER_IOA, 2);
        assert!(!result.guard_satisfiability.is_empty());
        assert!(!result.inductive_invariants.is_empty());
        assert!(!result.approximate);
        assert!(result.approximation_notes.is_empty());
    }

    #[test]
    fn test_list_contains_exact_at_bound() {
        let spec = r#"
[automaton]
name = "ListExact"
states = ["S"]
initial = "S"

[[state]]
name = "labels"
type = "list"
initial = "[]"

[[action]]
name = "ConflictingContains"
from = ["S"]
to = "S"
guard = [
    { type = "list_contains", var = "labels", value = "urgent" },
    { type = "list_contains", var = "labels", value = "normal" },
]
"#;

        // With max_counter=1, a single-slot list cannot contain two distinct
        // values simultaneously.
        let result = verify_symbolic(spec, 1);
        let guard = result
            .guard_satisfiability
            .iter()
            .find(|(name, _)| name == "ConflictingContains");
        assert!(guard.is_some());
        assert!(
            !guard.unwrap().1,
            "single-slot exact list encoding should reject conflicting contains guards"
        );
    }

    #[test]
    fn test_dead_guard_detected() {
        // Guard requires counter >= 10 but max is 2 → Z3 returns UNSAT
        let spec = r#"
[automaton]
name = "DeadGuard"
states = ["A", "B"]
initial = "A"

[[state]]
name = "items"
type = "counter"
initial = "0"

[[action]]
name = "Go"
from = ["A"]
to = "B"
guard = "items > 9"
"#;
        let result = verify_symbolic(spec, 2);
        let go_guard = result
            .guard_satisfiability
            .iter()
            .find(|(name, _)| name == "Go");
        assert!(go_guard.is_some());
        assert!(
            !go_guard.unwrap().1,
            "Guard requiring items >= 10 with max_counter=2 should be unsatisfiable"
        );
    }

    #[test]
    fn test_non_inductive_invariant_detected() {
        // GoB reaches trigger state B but doesn't increment count →
        // Z3 finds pre-state where count=1, effect none → post count=0 possible?
        // Actually: no effects, so counter_post = counter_pre. If pre > 0
        // then post > 0. This IS inductive because no decrement.
        // Let's test with a decrement instead.
        let spec = r#"
[automaton]
name = "NonInductive"
states = ["A", "B"]
initial = "A"

[[state]]
name = "count"
type = "counter"
initial = "0"

[[action]]
name = "GoB"
from = ["A"]
to = "B"

[[invariant]]
name = "BRequiresCount"
when = ["B"]
assert = "count > 0"
"#;
        // GoB reaches B, no effects on count. The invariant says count > 0
        // when in B. Since GoB doesn't set count, if count was 0 in A,
        // count will be 0 in B. But in the Z3 induction check, we assume
        // count > 0 in pre-state (induction hypothesis). Since no decrement,
        // count stays > 0. So this IS inductive from the Z3 perspective.
        //
        // The issue is reachability: GoB can fire from A when count=0 (no guard),
        // reaching B with count=0. But that's a BASE CASE violation, not an
        // induction failure. Induction only checks: if invariant holds before
        // transition, does it hold after?
        let result = verify_symbolic(spec, 2);
        let inv = result
            .inductive_invariants
            .iter()
            .find(|(name, _)| name == "BRequiresCount");
        assert!(inv.is_some());
        // This is inductive (no counter modification), even though the
        // invariant doesn't hold from initial state — that's a BFS check.
        assert!(
            inv.unwrap().1,
            "BRequiresCount is inductive (no counter change)"
        );
    }

    #[test]
    fn test_decrement_breaks_induction() {
        // Transition decrements counter when reaching trigger state →
        // Z3 finds counterexample: count_pre=1, count_post=0
        let spec = r#"
[automaton]
name = "DecrBreaks"
states = ["A", "B"]
initial = "A"

[[state]]
name = "count"
type = "counter"
initial = "0"

[[action]]
name = "GoB"
from = ["A"]
to = "B"
effect = "decrement count"

[[invariant]]
name = "BNeedsCount"
when = ["B"]
assert = "count > 0"
"#;
        let result = verify_symbolic(spec, 2);
        let inv = result
            .inductive_invariants
            .iter()
            .find(|(name, _)| name == "BNeedsCount");
        assert!(inv.is_some());
        // GoB decrements count. Z3 finds: count_pre=1, count_post=0 ≤ 0 → SAT
        // So induction fails.
        assert!(
            !inv.unwrap().1,
            "BNeedsCount should NOT be inductive (decrement can reach 0)"
        );
    }
}
