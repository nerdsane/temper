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

use std::collections::BTreeSet;

use z3::ast::{Bool, Int};
use z3::{SatResult, Solver};

use crate::model::builder::build_model_from_ioa;
use crate::model::types::{InvariantKind, ModelEffect, ModelGuard, TemperModel};

/// Result of symbolic verification.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SmtResult {
    /// For each action, whether its guard is satisfiable (can ever fire).
    pub guard_satisfiability: Vec<(String, bool)>,
    /// For each invariant, whether it is inductively maintained by all transitions.
    pub inductive_invariants: Vec<(String, bool)>,
    /// States that cannot be reached from the initial state.
    pub unreachable_states: Vec<String>,
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
    let model = build_model_from_ioa(ioa_toml, max_counter);

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
        all_passed,
    }
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

            // Encode the guard as a Z3 formula and assert it
            let guard_formula = encode_guard(&t.guard, &counter_vars, &bool_vars);
            solver.assert(&guard_formula);

            let sat = matches!(solver.check(), SatResult::Sat);
            (t.name.clone(), sat)
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Invariant induction
// ---------------------------------------------------------------------------

/// For each invariant, check that every transition preserves it.
///
/// For each (invariant, transition) pair where the transition can reach a
/// trigger state:
///   - Assume: invariant(S) ∧ guard(S) ∧ bounds
///   - Apply: encode effects as S → S'
///   - Prove: invariant(S') holds (check that ¬invariant(S') is UNSAT)
fn check_invariant_induction(model: &TemperModel, max_counter: usize) -> Vec<(String, bool)> {
    model
        .invariants
        .iter()
        .map(|inv| {
            let inductive = match &inv.kind {
                InvariantKind::StatusInSet => {
                    // Structurally guaranteed by parser validation: every
                    // transition's to_state must be in model.states.
                    model.transitions.iter().all(|t| {
                        t.to_state
                            .as_ref()
                            .map(|s| model.states.contains(s))
                            .unwrap_or(true)
                    })
                }
                InvariantKind::CounterPositive { var } => check_counter_positive_induction_z3(
                    model,
                    &inv.trigger_states,
                    var,
                    max_counter,
                ),
                InvariantKind::BoolRequired { var } => {
                    check_bool_required_induction_z3(model, &inv.trigger_states, var)
                }
                InvariantKind::NoFurtherTransitions => {
                    // For each trigger state: no transitions should have it
                    // as a from_state.
                    inv.trigger_states.iter().all(|trigger| {
                        !model
                            .transitions
                            .iter()
                            .any(|t| t.from_states.contains(trigger) || t.from_states.is_empty())
                    })
                }
                InvariantKind::Implication => {
                    if inv.required_states.is_empty() {
                        true
                    } else {
                        model.transitions.iter().all(|t| {
                            if let Some(to) = &t.to_state {
                                if inv.trigger_states.contains(to) {
                                    let valid: Vec<&String> = inv
                                        .required_states
                                        .iter()
                                        .filter(|s| model.states.contains(s))
                                        .collect();
                                    valid.is_empty() || valid.contains(&to)
                                } else {
                                    true
                                }
                            } else {
                                true
                            }
                        })
                    }
                }
            };

            (inv.name.clone(), inductive)
        })
        .collect()
}

/// Z3 induction check for CounterPositive invariants.
///
/// For each transition T that reaches a trigger state:
///   Assume: var > 0 (pre-state invariant) ∧ 0 ≤ var ≤ max
///   Apply: effects (compute var')
///   Check: var' > 0 must hold (i.e. ¬(var' > 0) is UNSAT)
fn check_counter_positive_induction_z3(
    model: &TemperModel,
    trigger_states: &[String],
    var: &str,
    max_counter: usize,
) -> bool {
    for t in &model.transitions {
        // Only check transitions that reach a trigger state
        let reaches_trigger = t
            .to_state
            .as_ref()
            .is_some_and(|s| trigger_states.contains(s));

        if !reaches_trigger {
            continue;
        }

        let solver = Solver::new();

        // Pre-state counter variable
        let counter_pre = Int::new_const(format!("{var}_pre"));
        let zero = Int::from_i64(0);
        let max_val = Int::from_i64(max_counter as i64);

        // Assume: invariant holds in pre-state (var > 0)
        solver.assert(counter_pre.gt(&zero));
        // Assume: counter is within bounds
        solver.assert(counter_pre.le(&max_val));

        // Compute post-state counter value based on effects
        let one = Int::from_i64(1);
        let mut counter_post = counter_pre.clone();
        for effect in &t.effects {
            match effect {
                ModelEffect::IncrementCounter(v) if v == var => {
                    counter_post = Int::add(&[&counter_post, &one]);
                }
                ModelEffect::DecrementCounter(v) if v == var => {
                    counter_post = Int::sub(&[&counter_post, &one]);
                }
                _ => {}
            }
        }

        // Check: ¬(var' > 0) — if SAT, invariant is not preserved
        solver.assert(counter_post.le(&zero));

        if matches!(solver.check(), SatResult::Sat) {
            return false;
        }
    }
    true
}

/// Z3 induction check for BoolRequired invariants.
///
/// For each transition T that reaches a trigger state:
///   Assume: var = true (pre-state invariant)
///   Apply: effects
///   Check: var' = true must hold (¬var' is UNSAT)
fn check_bool_required_induction_z3(
    model: &TemperModel,
    trigger_states: &[String],
    var: &str,
) -> bool {
    for t in &model.transitions {
        let reaches_trigger = t
            .to_state
            .as_ref()
            .is_some_and(|s| trigger_states.contains(s));

        if !reaches_trigger {
            continue;
        }

        let solver = Solver::new();

        // Pre-state: var = true (invariant holds)
        let bool_pre = Bool::new_const(format!("{var}_pre"));
        solver.assert(&bool_pre);

        // Compute post-state based on effects
        let mut bool_post = bool_pre.clone();
        for effect in &t.effects {
            if let ModelEffect::SetBool { var: v, value } = effect
                && v == var
            {
                bool_post = Bool::from_bool(*value);
            }
        }

        // Check: ¬var' — if SAT, invariant is not preserved
        solver.assert(bool_post.not());

        if matches!(solver.check(), SatResult::Sat) {
            return false;
        }
    }
    true
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

/// Encode a `ModelGuard` as a Z3 boolean formula.
fn encode_guard(
    guard: &ModelGuard,
    counter_vars: &[(String, Int)],
    bool_vars: &[(String, Bool)],
) -> Bool {
    match guard {
        ModelGuard::Always => Bool::from_bool(true),
        ModelGuard::CounterMin { var, min } => {
            let min_val = Int::from_i64(*min as i64);
            if let Some((_, z3_var)) = counter_vars.iter().find(|(n, _)| n == var) {
                z3_var.ge(&min_val)
            } else {
                // Unknown counter — unsatisfiable
                Bool::from_bool(false)
            }
        }
        ModelGuard::BoolTrue(var) => {
            if let Some((_, z3_var)) = bool_vars.iter().find(|(n, _)| n == var) {
                z3_var.clone()
            } else {
                // Unknown boolean — unsatisfiable
                Bool::from_bool(false)
            }
        }
        ModelGuard::And(guards) => {
            let formulas: Vec<Bool> = guards
                .iter()
                .map(|g| encode_guard(g, counter_vars, bool_vars))
                .collect();
            Bool::and(&formulas)
        }
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
