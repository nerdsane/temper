//! A move's chosen inputs, read in the state before its effects.

use std::collections::{BTreeMap, BTreeSet};

use temper_spec::automaton::{Action, ActionConstraint};
use temper_spec::predicate::{CmpOp, Env, Expr, Literal, Operand, Set, Val, eval};

use super::assignments::param_assignments_budgeted;
use super::count_bounds::CountRange;
use super::heap::{Heap, tree_bytes};
use super::semantics::{Expansion, ModelEnv, evaluate_guard};
use super::strings::string_assignments;
use super::types::{ModelString, ParamValue, ResolvedTransition, TemperModel, TemperModelState};

/// State and parameters shared by guard and entry-rule evaluation.
pub(super) struct MoveEnv<'a> {
    pub(super) local: ModelEnv<'a>,
    pub(super) params: &'a BTreeMap<String, ParamValue>,
}

impl Env for MoveEnv<'_> {
    fn status(&self) -> Val<'_> {
        self.local.status()
    }

    fn var(&self, name: &str) -> Val<'_> {
        self.local.var(name)
    }

    fn param(&self, name: &str) -> Val<'_> {
        match self.params.get(name) {
            Some(ParamValue::Count(n)) => Val::Num(*n as f64),
            Some(ParamValue::Bool(value)) => Val::Bool(*value),
            Some(ParamValue::Str(value)) => value.val(),
            Some(ParamValue::Fresh) | None => Val::Unknown,
        }
    }

    fn cross_statuses(&self, _: &str, _: &str) -> Option<Vec<Val<'_>>> {
        None
    }
}

/// Whether chosen inputs are checked against the move's guard. Candidate
/// ranges must preserve guard distinctions whenever this check runs.
pub(super) fn checks_inputs(model: &TemperModel, t: &ResolvedTransition) -> bool {
    !model.entry_rules.is_empty() || !t.string_params.is_empty()
}

/// Whether these inputs can satisfy the move's guard and comparison
/// constraints. String nonempty constraints are decided when inputs are made.
pub(super) fn inputs_allow(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
    params: &BTreeMap<String, ParamValue>,
) -> bool {
    eval(
        &t.guard,
        &MoveEnv {
            local: ModelEnv {
                kinds: &model.var_kinds,
                state,
            },
            params,
        },
    )
    .may_hold()
}

/// Keep comparison constraints in the same predicate as the guard, so an
/// assignment cannot satisfy them independently. Nonempty is a string-input
/// property and is already enforced by `string_candidates`.
pub(super) fn guard_with_constraints(action: &Action) -> Expr {
    let mut parts = vec![action.guard.clone()];
    for constraint in &action.constraints {
        let op = match constraint {
            ActionConstraint::ParamEqualsField { .. } => CmpOp::Eq,
            ActionConstraint::ParamNotEqualsField { .. } => CmpOp::Ne,
            ActionConstraint::ParamGreaterThanField { .. } => CmpOp::Gt,
            ActionConstraint::ParamNonempty { .. } => continue,
        };
        parts.push(Expr::Compare {
            lhs: Operand::Param(constraint.param().to_string()),
            op,
            rhs: Operand::Var(constraint.field().expect("comparison field").to_string()),
        });
    }
    Expr::And(parts)
}

/// Effects can collapse inputs that a guard distinguishes. Follow comparisons
/// from this input (including other inputs it is compared with), and explore
/// through their literals, local bounds and other inputs' effect ranges,
/// plus one distinct value per input.
/// Unrelated state and independent comparisons cannot widen this range.
pub(super) fn guard_count_bound(
    model: &TemperModel,
    t: &ResolvedTransition,
    param: &str,
    effect_ranges: &BTreeMap<&str, CountRange>,
) -> usize {
    fn literal(value: &Literal) -> usize {
        match value {
            Literal::Int(n) => usize::try_from(*n).unwrap_or(0),
            _ => 0,
        }
    }
    fn operand(model: &TemperModel, value: &Operand) -> usize {
        match value {
            Operand::Lit(value) => literal(value),
            Operand::Var(name) => model.counter_bounds.get(name).copied().unwrap_or(0),
            Operand::Len(name) => model.list_bounds.get(name).copied().unwrap_or(0),
            _ => 0,
        }
    }
    fn connected(value: &Operand, params: &BTreeSet<String>) -> bool {
        matches!(value, Operand::Param(name) if params.contains(name))
    }
    fn inspect(expr: &Expr, model: &TemperModel, params: &mut BTreeSet<String>, bound: &mut usize) {
        match expr {
            Expr::Compare { lhs, rhs, .. } if connected(lhs, params) || connected(rhs, params) => {
                *bound = (*bound).max(operand(model, lhs)).max(operand(model, rhs));
                for value in [lhs, rhs] {
                    if let Operand::Param(name) = value {
                        params.insert(name.clone());
                    }
                }
            }
            Expr::In {
                value,
                set: Set::List(items),
                ..
            } if connected(value, params) => {
                *bound = (*bound).max(items.iter().map(literal).max().unwrap_or(0));
            }
            Expr::And(parts) | Expr::Or(parts) => {
                for part in parts {
                    inspect(part, model, params, bound);
                }
            }
            Expr::Implies(a, b) => {
                inspect(a, model, params, bound);
                inspect(b, model, params, bound);
            }
            Expr::Not(inner) => inspect(inner, model, params, bound),
            _ => {}
        }
    }
    let mut params = BTreeSet::from([param.to_string()]);
    let mut bound = 0;
    loop {
        let previous = params.len();
        inspect(&t.guard, model, &mut params, &mut bound);
        if params.len() == previous {
            let effects = params
                .iter()
                .filter(|p| p.as_str() != param)
                .filter_map(|p| effect_ranges.get(p.as_str()))
                .map(|range| range.largest)
                .max()
                .unwrap_or(0);
            return bound.max(effects).saturating_add(params.len());
        }
    }
}

/// Visit chosen inputs before effects or state bounds are applied, stopping
/// when `visit` returns true. Exploration and local enablement share choices.
pub(super) fn any_inputs(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
    values: &BTreeSet<ModelString>,
    budget: &Expansion,
    mut visit: impl FnMut(BTreeMap<String, ParamValue>) -> bool,
) -> bool {
    let strings = string_assignments(model, t, state, values, budget);
    let numbers = param_assignments_budgeted(model, t, budget);
    let temporary = strings.heap() + numbers.heap();
    let mut found = false;
    'inputs: for params in &numbers {
        for chosen in &strings {
            if !budget.take() {
                break 'inputs;
            }
            let nodes = tree_bytes::<String, ParamValue>(params.len() + chosen.len());
            let entries = params
                .iter()
                .map(|(k, v)| k.heap() + v.heap())
                .sum::<usize>()
                + chosen
                    .iter()
                    .map(|(k, v)| k.heap() + v.heap())
                    .sum::<usize>();
            if !budget.hold(nodes + entries) {
                break 'inputs;
            }
            let mut params = params.clone();
            params.extend(
                chosen
                    .iter()
                    .map(|(k, v)| (k.clone(), ParamValue::Str(v.clone()))),
            );
            if visit(params) {
                found = true;
                break 'inputs;
            }
        }
    }
    drop(strings);
    drop(numbers);
    budget.give(temporary);
    found
}

/// Whether some allowed input makes the guard definitely true locally.
/// Related-status gates remain unknown: the record may still need to wait.
/// Effects going past an exploration bound do not disable a runtime move.
/// None means the input search exhausted its budget; it proves neither
/// enablement nor a deadlock.
pub(super) fn locally_enabled(
    model: &TemperModel,
    t: &ResolvedTransition,
    state: &TemperModelState,
) -> Option<bool> {
    if !checks_inputs(model, t) {
        return Some(evaluate_guard(&t.guard, &model.var_kinds, state));
    }
    let values = super::strings::string_values(std::iter::once(state));
    let budget = Expansion::for_model(model);
    let enabled = any_inputs(model, t, state, &values, &budget, |params| {
        eval(
            &t.guard,
            &MoveEnv {
                local: ModelEnv {
                    kinds: &model.var_kinds,
                    state,
                },
                params: &params,
            },
        )
        .must_hold()
    });
    (!budget.exhausted.get()).then_some(enabled)
}
