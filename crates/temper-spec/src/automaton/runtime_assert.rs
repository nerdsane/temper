//! Runtime-enforced safety assertions shared by production and deterministic simulation.

use std::collections::{BTreeMap, BTreeSet};

use super::{AssertCompareOp, Automaton, ParsedAssert, parse_assert_expr};

struct SafetyDeclarations {
    bool_names: BTreeSet<String>,
    counter_names: BTreeSet<String>,
    string_names: BTreeSet<String>,
    status_names: BTreeSet<String>,
}

impl SafetyDeclarations {
    fn from_automaton(automaton: &Automaton) -> Self {
        Self {
            bool_names: declared_names(automaton, "bool"),
            counter_names: declared_names(automaton, "counter"),
            string_names: declared_names(automaton, "string"),
            status_names: automaton.automaton.states.iter().cloned().collect(),
        }
    }

    fn supports(&self, when: &[String], assertion: &str) -> bool {
        when.iter().all(|state| self.status_names.contains(state))
            && parse_assert_expr(assertion).is_some_and(|parsed| {
                parsed.is_supported_safety_assertion(
                    &self.bool_names,
                    &self.counter_names,
                    &self.string_names,
                    &self.status_names,
                )
            })
    }
}

/// Version of the runtime-invariant enforcement contract.
pub const RUNTIME_INVARIANT_ENFORCEMENT_VERSION: u32 = 2;

/// Assertion forms whose safety is enforced on tentative runtime state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RuntimeAssert {
    /// An assertion that always holds.
    Always,
    /// A declared counter must remain positive.
    CounterPositive { var: String },
    /// Compare a declared counter with a literal bound.
    CounterCompare {
        var: String,
        op: AssertCompareOp,
        value: usize,
    },
    /// A declared string field must contain at least one character.
    StringNonEmpty { var: String },
    /// Compare one declared counter with another.
    CounterVarCompare {
        left: String,
        op: AssertCompareOp,
        right: String,
    },
    /// A declared boolean must equal the expected value.
    BoolRequired { var: String, expect: bool },
    /// The entity must not enter the named state.
    NeverState { state: String },
    /// Every nested assertion must hold.
    And(Vec<RuntimeAssert>),
    /// At least one nested assertion must hold.
    Or(Vec<RuntimeAssert>),
}

/// A named runtime-enforced assertion and its activating states.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeInvariant {
    /// Declared invariant name.
    pub name: String,
    /// States in which the assertion applies; empty means every state.
    pub when: Vec<String>,
    /// Typed runtime assertion.
    pub assertion: RuntimeAssert,
    /// Stable enforcement contract version attached at compilation.
    pub enforcement_version: u32,
}

/// Compile the runtime-enforced subset of an automaton's safety assertions.
pub fn compile_runtime_invariants(automaton: &Automaton) -> Vec<RuntimeInvariant> {
    let declarations = SafetyDeclarations::from_automaton(automaton);

    automaton
        .invariants
        .iter()
        .filter_map(|invariant| {
            let parsed = parse_assert_expr(&invariant.assert)?;
            if !declarations.supports(&invariant.when, &invariant.assert) {
                return None;
            }
            let inherently_runtime_enforced = matches!(
                parsed,
                ParsedAssert::StringNonEmpty { .. } | ParsedAssert::CounterVarCompare { .. }
            );
            if !inherently_runtime_enforced
                && !requires_parameter_runtime_enforcement(&parsed, &invariant.when, automaton)
            {
                return None;
            }
            let assertion = compile_runtime_assert(parsed)?;
            Some(RuntimeInvariant {
                name: invariant.name.clone(),
                when: invariant.when.clone(),
                assertion,
                enforcement_version: RUNTIME_INVARIANT_ENFORCEMENT_VERSION,
            })
        })
        .collect()
}

fn compile_runtime_assert(parsed: ParsedAssert) -> Option<RuntimeAssert> {
    Some(match parsed {
        ParsedAssert::Always => RuntimeAssert::Always,
        ParsedAssert::CounterPositive { var } => RuntimeAssert::CounterPositive { var },
        ParsedAssert::CounterCompare { var, op, value } => {
            RuntimeAssert::CounterCompare { var, op, value }
        }
        ParsedAssert::StringNonEmpty { var } => RuntimeAssert::StringNonEmpty { var },
        ParsedAssert::CounterVarCompare { left, op, right } => {
            RuntimeAssert::CounterVarCompare { left, op, right }
        }
        ParsedAssert::BoolRequired { var, expect } => RuntimeAssert::BoolRequired { var, expect },
        ParsedAssert::NeverState { state } => RuntimeAssert::NeverState { state },
        ParsedAssert::NoFurtherTransitions => return None,
        ParsedAssert::And(parts) => RuntimeAssert::And(
            parts
                .into_iter()
                .map(compile_runtime_assert)
                .collect::<Option<Vec<_>>>()?,
        ),
        ParsedAssert::Or(parts) => RuntimeAssert::Or(
            parts
                .into_iter()
                .map(compile_runtime_assert)
                .collect::<Option<Vec<_>>>()?,
        ),
        ParsedAssert::OrderingConstraint { .. } => return None,
    })
}

/// Return every declared safety invariant that cannot be enforced by the
/// shared verification/runtime capability contract.
///
/// Registry activation uses this dependency-light preflight so unsupported
/// specs cannot become live even when higher verification layers are bypassed.
pub fn unsupported_safety_invariant_names(automaton: &Automaton) -> Vec<String> {
    let declarations = SafetyDeclarations::from_automaton(automaton);
    automaton
        .invariants
        .iter()
        .filter(|invariant| {
            !declarations.supports(&invariant.when, &invariant.assert)
                || parse_assert_expr(&invariant.assert).is_some_and(|parsed| {
                    requires_parameter_runtime_enforcement(&parsed, &invariant.when, automaton)
                        && depends_on_no_further_transitions(&parsed)
                })
        })
        .map(|invariant| invariant.name.clone())
        .collect()
}

/// Return declared counter and boolean variables whose values are governed by
/// model-proved invariants, including assertions that are also enforced at
/// runtime because transition parameters are outside the finite model.
///
/// Caller payloads must not mutate these variables directly. Their values are
/// changed only by transition effects, while runtime enforcement rejects any
/// parameter-derived result outside the state space proved by verification.
/// Protection covers the complete logical model state whenever any invariant
/// depends on model reachability: guard-only variables can otherwise make a
/// transition reachable in production that was unreachable during proof.
pub fn model_protected_state_var_names(automaton: &Automaton) -> BTreeSet<String> {
    let declarations = SafetyDeclarations::from_automaton(automaton);
    let has_model_proved_invariant = automaton.invariants.iter().any(|invariant| {
        declarations.supports(&invariant.when, &invariant.assert)
            && parse_assert_expr(&invariant.assert).is_some_and(|assertion| {
                !matches!(
                    assertion,
                    ParsedAssert::StringNonEmpty { .. } | ParsedAssert::CounterVarCompare { .. }
                )
            })
    });
    if has_model_proved_invariant {
        declarations
            .bool_names
            .union(&declarations.counter_names)
            .cloned()
            .collect()
    } else {
        BTreeSet::new()
    }
}

fn declared_names(automaton: &Automaton, var_type: &str) -> BTreeSet<String> {
    automaton
        .state
        .iter()
        .filter(|state| state.var_type == var_type)
        .map(|state| state.name.clone())
        .collect()
}

/// Evaluate a typed runtime assertion against tentative entity state.
pub fn evaluate_runtime_assert(
    assertion: &RuntimeAssert,
    status: &str,
    counters: &BTreeMap<String, usize>,
    booleans: &BTreeMap<String, bool>,
    fields: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    match assertion {
        RuntimeAssert::Always => true,
        RuntimeAssert::CounterPositive { var } => counters.get(var).copied().unwrap_or(0) > 0,
        RuntimeAssert::CounterCompare { var, op, value } => {
            compare_counter(counters.get(var).copied().unwrap_or(0), op, *value)
        }
        RuntimeAssert::StringNonEmpty { var } => fields.get(var).is_some_and(|value| {
            value.as_str().is_some_and(|value| !value.is_empty())
                || value.as_object().is_some_and(|object| {
                    object
                        .get("__temper_blob_ref")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|reference| !reference.is_empty())
                        && object
                            .get("__temper_blob_encoding")
                            .and_then(serde_json::Value::as_str)
                            == Some("json")
                })
        }),
        RuntimeAssert::CounterVarCompare { left, op, right } => {
            let left = counters.get(left).copied().unwrap_or(0);
            let right = counters.get(right).copied().unwrap_or(0);
            compare_counter(left, op, right)
        }
        RuntimeAssert::BoolRequired { var, expect } => {
            booleans.get(var).copied().unwrap_or(false) == *expect
        }
        RuntimeAssert::NeverState { state } => status != state,
        RuntimeAssert::And(parts) => parts
            .iter()
            .all(|part| evaluate_runtime_assert(part, status, counters, booleans, fields)),
        RuntimeAssert::Or(parts) => parts
            .iter()
            .any(|part| evaluate_runtime_assert(part, status, counters, booleans, fields)),
    }
}

fn requires_parameter_runtime_enforcement(
    assertion: &ParsedAssert,
    active_states: &[String],
    automaton: &Automaton,
) -> bool {
    if assertion_is_tautology(assertion) {
        return false;
    }
    let parameter_counters: BTreeSet<&str> = automaton
        .actions
        .iter()
        .filter(|action| action.kind != "output")
        .flat_map(|action| &action.effect)
        .filter_map(|effect| match effect {
            super::Effect::Increment {
                var,
                amount: Some(_),
            }
            | super::Effect::Decrement {
                var,
                amount: Some(_),
            }
            | super::Effect::SetCounterFromParam { var, .. } => Some(var.as_str()),
            _ => None,
        })
        .collect();
    if parameter_counters.is_empty() {
        return false;
    }

    assertion_references_any_counter(assertion, &parameter_counters)
        || automaton
            .actions
            .iter()
            .filter(|action| action.kind != "output")
            .filter(|action| {
                if !is_pure_terminal_assertion(assertion) {
                    return true;
                }
                active_states.is_empty()
                    || action.from.is_empty()
                    || action
                        .from
                        .iter()
                        .any(|state| active_states.contains(state))
            })
            .flat_map(|action| &action.guard)
            .any(|guard| match guard {
                super::Guard::MinCount { var, .. } | super::Guard::MaxCount { var, .. } => {
                    parameter_counters.contains(var.as_str())
                }
                _ => false,
            })
}

fn assertion_references_any_counter(assertion: &ParsedAssert, counters: &BTreeSet<&str>) -> bool {
    match assertion {
        ParsedAssert::CounterPositive { var } | ParsedAssert::CounterCompare { var, .. } => {
            counters.contains(var.as_str())
        }
        ParsedAssert::CounterVarCompare { left, right, .. } => {
            counters.contains(left.as_str()) || counters.contains(right.as_str())
        }
        ParsedAssert::And(parts) | ParsedAssert::Or(parts) => parts
            .iter()
            .any(|part| assertion_references_any_counter(part, counters)),
        _ => false,
    }
}

fn depends_on_no_further_transitions(assertion: &ParsedAssert) -> bool {
    if assertion_is_tautology(assertion) {
        return false;
    }
    match assertion {
        ParsedAssert::NoFurtherTransitions => true,
        ParsedAssert::And(parts) | ParsedAssert::Or(parts) => {
            parts.iter().any(depends_on_no_further_transitions)
        }
        _ => false,
    }
}

fn is_pure_terminal_assertion(assertion: &ParsedAssert) -> bool {
    depends_on_no_further_transitions(assertion) && !contains_nonterminal_safety_leaf(assertion)
}

fn assertion_is_tautology(assertion: &ParsedAssert) -> bool {
    match assertion {
        ParsedAssert::Always => true,
        ParsedAssert::And(parts) => parts.iter().all(assertion_is_tautology),
        ParsedAssert::Or(parts) => parts.iter().any(assertion_is_tautology),
        _ => false,
    }
}

fn contains_nonterminal_safety_leaf(assertion: &ParsedAssert) -> bool {
    if assertion_is_tautology(assertion) {
        return false;
    }
    match assertion {
        ParsedAssert::Always | ParsedAssert::NoFurtherTransitions => false,
        ParsedAssert::And(parts) | ParsedAssert::Or(parts) => {
            parts.iter().any(contains_nonterminal_safety_leaf)
        }
        _ => true,
    }
}

fn compare_counter(left: usize, op: &AssertCompareOp, right: usize) -> bool {
    match op {
        AssertCompareOp::Gt => left > right,
        AssertCompareOp::Gte => left >= right,
        AssertCompareOp::Lt => left < right,
        AssertCompareOp::Lte => left <= right,
        AssertCompareOp::Eq => left == right,
    }
}

#[cfg(test)]
#[path = "runtime_assert_test.rs"]
mod tests;
