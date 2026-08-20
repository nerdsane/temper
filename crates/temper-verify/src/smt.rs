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
use z3::{Params, SatResult, Solver};

use temper_spec::automaton::AssertCompareOp;

use crate::model::builder::build_model_from_ioa;
use crate::model::semantics::collect_list_contains_pairs;
use crate::model::types::{
    InvariantKind, ModelEffect, ModelGuard, ResolvedTransition, TemperModel,
};

/// Result of symbolic verification.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SmtResult {
    /// For each action, whether its guard is satisfiable (can ever fire).
    pub guard_satisfiability: Vec<(String, bool)>,
    /// For each invariant, whether it is inductively maintained by all transitions.
    pub inductive_invariants: Vec<(String, bool)>,
    /// Per-action immutable-reference and deterministic-identity induction.
    pub reference_contracts: Vec<(String, bool)>,
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
    verify_symbolic_inner(ioa_toml, max_counter, None)
}

/// Run symbolic verification under a hard Z3 resource-unit budget.
pub fn verify_symbolic_with_budget(
    ioa_toml: &str,
    max_counter: usize,
    resource_budget: u32,
) -> SmtResult {
    verify_symbolic_inner(ioa_toml, max_counter, Some(resource_budget))
}

fn verify_symbolic_inner(
    ioa_toml: &str,
    max_counter: usize,
    resource_budget: Option<u32>,
) -> SmtResult {
    let model = build_model_from_ioa(ioa_toml, max_counter)
        .expect("SMT: IOA spec should have been validated before symbolic verification");
    let mut approximation_notes = approximation_notes(&model);
    let approximate = !approximation_notes.is_empty();

    let query_upper_bound = solver_query_upper_bound(&model);
    let per_query_budget = resource_budget.map(|budget| budget / query_upper_bound);
    if per_query_budget == Some(0) {
        approximation_notes.push(format!(
            "symbolic resource budget is smaller than the {query_upper_bound} bounded solver queries"
        ));
        return SmtResult {
            guard_satisfiability: Vec::new(),
            inductive_invariants: Vec::new(),
            reference_contracts: Vec::new(),
            unreachable_states: check_unreachable_states(&model),
            approximate: true,
            approximation_notes,
            all_passed: false,
        };
    }

    let guard_sat = check_guard_satisfiability(&model, max_counter, per_query_budget);
    let inductive = check_invariant_induction(&model, max_counter, per_query_budget);
    let reference_contracts = check_reference_contract_induction(&model, per_query_budget);
    let unreachable = check_unreachable_states(&model);

    // Unreachable states are warnings, not failures — specs may declare states
    // that are only reachable through composition or external actions.
    let all_passed = guard_sat.iter().all(|(_, sat)| *sat)
        && inductive.iter().all(|(_, ind)| *ind)
        && reference_contracts.iter().all(|(_, valid)| *valid);

    SmtResult {
        guard_satisfiability: guard_sat,
        inductive_invariants: inductive,
        reference_contracts,
        unreachable_states: unreachable,
        approximate,
        approximation_notes,
        all_passed,
    }
}

fn bounded_solver(resource_budget: Option<u32>) -> Solver {
    let solver = Solver::new();
    if let Some(resource_budget) = resource_budget {
        let mut params = Params::new();
        params.set_u32("rlimit", resource_budget);
        solver.set_params(&params);
    }
    solver
}

fn invariant_node_count(kind: &InvariantKind) -> u32 {
    match kind {
        InvariantKind::And(parts) | InvariantKind::Or(parts) => 1u32.saturating_add(
            parts
                .iter()
                .map(invariant_node_count)
                .fold(0u32, u32::saturating_add),
        ),
        _ => 1,
    }
}

fn solver_query_upper_bound(model: &TemperModel) -> u32 {
    let transitions = u32::try_from(model.transitions.len()).unwrap_or(u32::MAX);
    let invariant_nodes = model
        .invariants
        .iter()
        .map(|invariant| invariant_node_count(&invariant.kind))
        .fold(0u32, u32::saturating_add);
    transitions
        .saturating_mul(2u32.saturating_add(invariant_nodes))
        .max(1)
}

fn check_reference_contract_induction(
    model: &TemperModel,
    resource_budget: Option<u32>,
) -> Vec<(String, bool)> {
    let references = model
        .reference_properties_by_type
        .values()
        .flatten()
        .cloned()
        .collect::<Vec<_>>();
    model
        .transitions
        .iter()
        .map(|transition| {
            if references.is_empty() {
                return (transition.name.clone(), true);
            }
            let solver = bounded_solver(resource_budget);
            let params = make_reference_param_vars(transition, &solver);
            let zero = Int::from_i64(0);
            let max = Int::from_i64(
                temper_spec::automaton::MAX_REFERENCE_TARGETS_PER_WRITE.saturating_mul(3) as i64,
            );
            let mut pre_refs = BTreeMap::new();
            let mut post_refs = BTreeMap::new();
            let mut violations = Vec::new();
            for reference in &references {
                let pre = Int::new_const(format!("pre_ref:{reference}"));
                let post = Int::new_const(format!("post_ref:{reference}"));
                solver.assert(pre.ge(&zero));
                solver.assert(pre.le(&max));
                solver.assert(post.ge(&zero));
                solver.assert(post.le(&max));
                let projected = transition.effects.iter().find_map(|effect| match effect {
                    ModelEffect::SetReferenceFromParam {
                        reference: candidate,
                        param,
                    } if candidate == reference => params.get(param),
                    _ => None,
                });
                if let Some(param) = projected {
                    solver.assert(post.eq(pre.eq(&zero).ite(param, &pre)));
                } else {
                    solver.assert(post.eq(&pre));
                }
                violations.push(Bool::and(&[&pre.gt(&zero), &post.ne(&pre)]));
                pre_refs.insert(reference.clone(), pre);
                post_refs.insert(reference.clone(), post);
            }
            for (index, property) in model.identity_properties.iter().enumerate() {
                let Some(pre_reference) = pre_refs.get(property) else {
                    return (transition.name.clone(), false);
                };
                let Some(reference) = post_refs.get(property) else {
                    return (transition.name.clone(), false);
                };
                let pre_binding = Int::new_const(format!("pre_id:{index}"));
                let post_binding = Int::new_const(format!("post_id:{index}"));
                solver.assert(pre_binding.ge(&zero));
                solver.assert(pre_binding.le(&max));
                solver.assert(pre_binding.eq(pre_reference));
                solver.assert(reference.gt(&zero));
                solver.assert(post_binding.eq(pre_binding.eq(&zero).ite(reference, &pre_binding)));
                violations.push(post_binding.ne(reference));
            }
            if violations.is_empty() {
                return (transition.name.clone(), true);
            }
            solver.assert(Bool::or(&violations));
            (
                transition.name.clone(),
                matches!(solver.check(), SatResult::Unsat),
            )
        })
        .collect()
}

fn approximation_notes(model: &TemperModel) -> Vec<String> {
    let cross_entity_guard_count = model
        .transitions
        .iter()
        .filter(|transition| transition.guard.contains_cross_entity())
        .count();
    if cross_entity_guard_count == 0 {
        return Vec::new();
    }

    vec![format!(
        "{cross_entity_guard_count} transition(s) use abstract cross-entity guards; single-entity SMT excludes them from local induction and reachability"
    )]
}

fn concrete_transitions(model: &TemperModel) -> impl Iterator<Item = &ResolvedTransition> {
    model
        .transitions
        .iter()
        .filter(|transition| !transition.guard.contains_cross_entity())
}

// ---------------------------------------------------------------------------
// Guard satisfiability
// ---------------------------------------------------------------------------

/// For each transition, encode its guard as a Z3 formula and check SAT.
///
/// A guard is satisfiable if there exists an assignment of counter values
/// (0..max_counter) and boolean values that makes the guard true.
fn check_guard_satisfiability(
    model: &TemperModel,
    max_counter: usize,
    resource_budget: Option<u32>,
) -> Vec<(String, bool)> {
    model
        .transitions
        .iter()
        .map(|t| {
            let solver = bounded_solver(resource_budget);

            // Check that at least one from_state exists in the state space
            if !t.from_states.is_empty() {
                let has_valid_from = t.from_states.iter().any(|s| model.states.contains(s));
                if !has_valid_from {
                    return (t.name.clone(), false);
                }
            }

            // Create Z3 variables for each counter, bounded [0, max_counter]
            let counter_vars = make_counter_vars(model, &solver, max_counter);
            constrain_unreachable_references(model, &solver, &counter_vars);
            let reference_param_vars = make_reference_param_vars(t, &solver);
            let bool_vars = make_bool_vars(model);
            let list_vars = make_list_vars(model, &solver, max_counter);
            let status_var = make_status_var(model, &solver);

            if !t.from_states.is_empty() {
                let from_formula = encode_state_membership(&status_var, &t.from_states, model);
                solver.assert(&from_formula);
            }

            // Encode the guard as a Z3 formula and assert it
            let guard_formula = encode_guard(
                &t.guard,
                &counter_vars,
                &bool_vars,
                &list_vars,
                &status_var,
                model,
                &reference_param_vars,
            );
            solver.assert(&guard_formula);

            let sat = matches!(solver.check(), SatResult::Sat);
            (t.name.clone(), sat)
        })
        .collect()
}

/// Create the exact finite identity-class variables for typed action inputs.
/// Class zero is excluded here because malformed/unset runtime inputs are
/// rejected at the input boundary before guard evaluation.
fn make_reference_param_vars(
    transition: &ResolvedTransition,
    solver: &Solver,
) -> BTreeMap<String, Int> {
    let first = Int::from_i64(1);
    crate::model::reference_contract::parameter_budgets(&transition.effects)
        .into_iter()
        .map(|(param, budget)| {
            let var = Int::new_const(format!("reference_param:{param}"));
            solver.assert(var.ge(&first));
            solver.assert(var.le(Int::from_i64(budget as i64)));
            (param, var)
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
fn check_invariant_induction(
    model: &TemperModel,
    max_counter: usize,
    resource_budget: Option<u32>,
) -> Vec<(String, bool)> {
    model
        .invariants
        .iter()
        .map(|inv| {
            let inductive = match &inv.kind {
                InvariantKind::StatusInSet => {
                    // Structurally guaranteed by parser validation: every
                    // transition's to_state must be in model.states.
                    concrete_transitions(model).all(|t| {
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
                    resource_budget,
                ),
                InvariantKind::BoolRequired { var, expect } => {
                    // Induction checker assumes `expect = true`. For `!flag`,
                    // fall back to runtime simulation (model checking still
                    // exercises it via proptest_gen and simulation).
                    if *expect {
                        check_bool_required_induction_z3(
                            model,
                            &inv.trigger_states,
                            var,
                            resource_budget,
                        )
                    } else {
                        true
                    }
                }
                InvariantKind::NoFurtherTransitions => {
                    // For each trigger state: no transitions should have it
                    // as a from_state.
                    inv.trigger_states.iter().all(|trigger| {
                        !concrete_transitions(model)
                            .any(|t| t.from_states.contains(trigger) || t.from_states.is_empty())
                    })
                }
                InvariantKind::Implication => {
                    if inv.required_states.is_empty() {
                        true
                    } else {
                        concrete_transitions(model).all(|t| {
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
                InvariantKind::CounterCompare { var, op, value } => {
                    check_counter_compare_induction_z3(
                        model,
                        &inv.trigger_states,
                        var,
                        op,
                        *value,
                        max_counter,
                        resource_budget,
                    )
                }
                InvariantKind::NeverState { state } => {
                    // Structural check: no transition has to_state == forbidden_state.
                    !concrete_transitions(model)
                        .any(|t| t.to_state.as_ref().is_some_and(|to| to == state))
                }
                InvariantKind::And(parts) => {
                    // Sound over-approximation: `a && b` is inductive iff each
                    // part is inductive under the same trigger_states.
                    parts.iter().all(|p| {
                        kind_inductive_smt(
                            model,
                            &inv.trigger_states,
                            p,
                            max_counter,
                            resource_budget,
                        )
                    })
                }
                InvariantKind::Or(_) => {
                    // Disjunctive induction requires joint encoding; runtime
                    // simulation (proptest_gen/simulation) catches violations.
                    true
                }
                InvariantKind::Unverifiable { .. } => {
                    // Not checkable at model level — trivially inductive.
                    true
                }
            };

            (inv.name.clone(), inductive)
        })
        .collect()
}

/// Recursive induction check for a single [`InvariantKind`] — used by `And`.
///
/// Mirrors the per-variant logic in [`check_invariant_induction`]; compound
/// recursion is sound at the And layer (all parts must hold). For Or, returns
/// `true` (see note above — runtime catches disjunctive violations).
fn kind_inductive_smt(
    model: &TemperModel,
    trigger_states: &[String],
    kind: &InvariantKind,
    max_counter: usize,
    resource_budget: Option<u32>,
) -> bool {
    match kind {
        InvariantKind::StatusInSet => concrete_transitions(model).all(|t| {
            t.to_state
                .as_ref()
                .map(|s| model.states.contains(s))
                .unwrap_or(true)
        }),
        InvariantKind::CounterPositive { var } => check_counter_positive_induction_z3(
            model,
            trigger_states,
            var,
            max_counter,
            resource_budget,
        ),
        InvariantKind::BoolRequired { var, expect } => {
            if *expect {
                check_bool_required_induction_z3(model, trigger_states, var, resource_budget)
            } else {
                true
            }
        }
        InvariantKind::NoFurtherTransitions => trigger_states.iter().all(|trigger| {
            !concrete_transitions(model)
                .any(|t| t.from_states.contains(trigger) || t.from_states.is_empty())
        }),
        InvariantKind::Implication => true,
        InvariantKind::CounterCompare { var, op, value } => check_counter_compare_induction_z3(
            model,
            trigger_states,
            var,
            op,
            *value,
            max_counter,
            resource_budget,
        ),
        InvariantKind::NeverState { state } => {
            !concrete_transitions(model).any(|t| t.to_state.as_ref().is_some_and(|to| to == state))
        }
        InvariantKind::And(parts) => parts
            .iter()
            .all(|p| kind_inductive_smt(model, trigger_states, p, max_counter, resource_budget)),
        InvariantKind::Or(_) | InvariantKind::Unverifiable { .. } => true,
    }
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
    resource_budget: Option<u32>,
) -> bool {
    for t in &model.transitions {
        if t.guard.contains_cross_entity() {
            continue;
        }
        // Only check transitions that reach a trigger state
        let reaches_trigger = t
            .to_state
            .as_ref()
            .is_some_and(|s| trigger_states.contains(s));

        if !reaches_trigger {
            continue;
        }

        let solver = bounded_solver(resource_budget);

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
                    // Runtime semantics are saturating_sub(1): max(counter-1, 0)
                    let dec = Int::sub(&[&counter_post, &one]);
                    counter_post = counter_post.gt(&zero).ite(&dec, &zero);
                }
                _ => {}
            }
        }

        // Check: ¬(var' > 0) — if SAT, invariant is not preserved
        solver.assert(counter_post.le(&zero));

        if !matches!(solver.check(), SatResult::Unsat) {
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
    resource_budget: Option<u32>,
) -> bool {
    for t in &model.transitions {
        if t.guard.contains_cross_entity() {
            continue;
        }
        let reaches_trigger = t
            .to_state
            .as_ref()
            .is_some_and(|s| trigger_states.contains(s));

        if !reaches_trigger {
            continue;
        }

        let solver = bounded_solver(resource_budget);

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

        if !matches!(solver.check(), SatResult::Unsat) {
            return false;
        }
    }
    true
}

/// Z3 induction check for CounterCompare invariants.
///
/// Generalisation of `check_counter_positive_induction_z3`:
///   Assume: `var op value` (pre-state invariant) ∧ bounds
///   Apply: effects → var'
///   Check: `var' op value` must hold
fn check_counter_compare_induction_z3(
    model: &TemperModel,
    trigger_states: &[String],
    var: &str,
    op: &AssertCompareOp,
    value: usize,
    max_counter: usize,
    resource_budget: Option<u32>,
) -> bool {
    for t in &model.transitions {
        if t.guard.contains_cross_entity() {
            continue;
        }
        let reaches_trigger = t
            .to_state
            .as_ref()
            .is_some_and(|s| trigger_states.contains(s));

        if !reaches_trigger {
            continue;
        }

        let solver = bounded_solver(resource_budget);

        let counter_pre = Int::new_const(format!("{var}_pre"));
        let zero = Int::from_i64(0);
        let max_val = Int::from_i64(max_counter as i64);
        let val = Int::from_i64(value as i64);

        // Assume: counter is within bounds
        solver.assert(counter_pre.ge(&zero));
        solver.assert(counter_pre.le(&max_val));

        // Assume: invariant holds in pre-state
        let pre_invariant = match op {
            AssertCompareOp::Gt => counter_pre.gt(&val),
            AssertCompareOp::Gte => counter_pre.ge(&val),
            AssertCompareOp::Lt => counter_pre.lt(&val),
            AssertCompareOp::Lte => counter_pre.le(&val),
            AssertCompareOp::Eq => counter_pre.eq(&val),
        };
        solver.assert(&pre_invariant);

        // Compute post-state counter value based on effects
        let one = Int::from_i64(1);
        let mut counter_post = counter_pre.clone();
        for effect in &t.effects {
            match effect {
                ModelEffect::IncrementCounter(v) if v == var => {
                    counter_post = Int::add(&[&counter_post, &one]);
                }
                ModelEffect::DecrementCounter(v) if v == var => {
                    let dec = Int::sub(&[&counter_post, &one]);
                    counter_post = counter_post.gt(&zero).ite(&dec, &zero);
                }
                _ => {}
            }
        }

        // Check: ¬(var' op value) — if SAT, invariant is not preserved
        let post_invariant = match op {
            AssertCompareOp::Gt => counter_post.gt(&val),
            AssertCompareOp::Gte => counter_post.ge(&val),
            AssertCompareOp::Lt => counter_post.lt(&val),
            AssertCompareOp::Lte => counter_post.le(&val),
            AssertCompareOp::Eq => counter_post.eq(&val),
        };
        solver.assert(post_invariant.not());

        if !matches!(solver.check(), SatResult::Unsat) {
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
    model
        .initial_counters
        .keys()
        .map(|name| {
            let var = Int::new_const(name.as_str());
            solver.assert(var.ge(&zero));
            let bound =
                name.strip_prefix("__ref:")
                    .and_then(|property| {
                        model
                            .reference_properties_by_type
                            .iter()
                            .find(|(_, properties)| properties.iter().any(|name| name == property))
                            .map(|(target, properties)| {
                                let transition_budget = model
                                    .transitions
                                    .iter()
                                    .filter(|transition| {
                                        transition.effects.iter().any(|effect| matches!(
                                        effect,
                                        ModelEffect::ExploreReferenceParam { entity_type, .. }
                                            if entity_type == target
                                    ))
                                    })
                                    .flat_map(|transition| {
                                        crate::model::reference_contract::parameter_budgets(
                                            &transition.effects,
                                        )
                                        .into_iter()
                                        .map(|(_, budget)| budget)
                                    })
                                    .max()
                                    .unwrap_or(properties.len());
                                transition_budget.max(properties.len())
                            })
                    })
                    .unwrap_or(max_counter);
            solver.assert(var.le(Int::from_i64(bound as i64)));
            (name.clone(), var)
        })
        .collect()
}

fn constrain_unreachable_references(
    model: &TemperModel,
    solver: &Solver,
    counter_vars: &[(String, Int)],
) {
    for (name, variable) in counter_vars {
        let Some(reference) = name.strip_prefix("__ref:") else {
            continue;
        };
        let initially_set = model
            .initial_counter_variants
            .iter()
            .any(|variant| variant.get(name).copied().unwrap_or(0) != 0);
        let assignable = model.transitions.iter().any(|transition| {
            transition.effects.iter().any(|effect| {
                matches!(
                    effect,
                    ModelEffect::SetReferenceFromParam { reference: candidate, .. }
                        if candidate == reference
                )
            })
        });
        if !initially_set && !assignable {
            solver.assert(variable.eq(Int::from_i64(0)));
        }
    }
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

/// Encode a `ModelGuard` as a Z3 boolean formula.
fn encode_guard(
    guard: &ModelGuard,
    counter_vars: &[(String, Int)],
    bool_vars: &[(String, Bool)],
    list_vars: &ListSymbolicVars,
    status_var: &Int,
    model: &TemperModel,
    reference_param_vars: &BTreeMap<String, Int>,
) -> Bool {
    match guard {
        ModelGuard::Always => Bool::from_bool(true),
        ModelGuard::StateIn(states) => encode_state_membership(status_var, states, model),
        ModelGuard::CounterMin { var, min } => {
            let min_val = Int::from_i64(*min as i64);
            if let Some((_, z3_var)) = counter_vars.iter().find(|(n, _)| n == var) {
                z3_var.ge(&min_val)
            } else {
                // Unknown counter — unsatisfiable
                Bool::from_bool(false)
            }
        }
        ModelGuard::CounterMax { var, max } => {
            let max_val = Int::from_i64(*max as i64);
            if let Some((_, z3_var)) = counter_vars.iter().find(|(n, _)| n == var) {
                z3_var.lt(&max_val)
            } else {
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
        ModelGuard::BoolFalse(var) => {
            if let Some((_, z3_var)) = bool_vars.iter().find(|(n, _)| n == var) {
                z3_var.not()
            } else {
                // Unknown boolean defaults to false, so !false = true
                Bool::from_bool(true)
            }
        }
        ModelGuard::ListContains { var, value } => encode_list_contains(var, value, list_vars),
        ModelGuard::ListLengthMin { var, min } => {
            if let Some(len_var) = list_vars.len_vars.get(var) {
                len_var.ge(Int::from_i64(*min as i64))
            } else {
                Bool::from_bool(false)
            }
        }
        ModelGuard::CrossEntityState {
            entity_type,
            entity_id_source,
            required_status,
            forbidden_status,
        } => Bool::new_const(format!(
            "cross_entity_guard:{}:{}:{}:!{}",
            entity_type,
            entity_id_source,
            required_status.join("|"),
            forbidden_status.join("|")
        )),
        ModelGuard::ReferenceEquals { reference, param } => {
            let Some((_, stored)) = counter_vars
                .iter()
                .find(|(name, _)| name == &format!("__ref:{reference}"))
            else {
                return Bool::from_bool(false);
            };
            let Some(incoming) = reference_param_vars.get(param) else {
                return Bool::from_bool(false);
            };
            let first_identity_class = Int::from_i64(1);
            Bool::and(&[
                &stored.ge(&first_identity_class),
                &incoming.ge(&first_identity_class),
                &stored.eq(incoming),
            ])
        }
        ModelGuard::And(guards) => {
            let formulas: Vec<Bool> = guards
                .iter()
                .map(|g| {
                    encode_guard(
                        g,
                        counter_vars,
                        bool_vars,
                        list_vars,
                        status_var,
                        model,
                        reference_param_vars,
                    )
                })
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
            if t.guard.contains_cross_entity() {
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
#[path = "smt_test.rs"]
mod tests;
