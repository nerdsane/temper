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

/// Version of the atomic runtime-invariant enforcement contract.
pub const RUNTIME_INVARIANT_ENFORCEMENT_VERSION: u32 = 1;

/// Assertion forms whose safety is enforced on tentative runtime state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum RuntimeAssert {
    /// A declared string field must contain at least one character.
    StringNonEmpty { var: String },
    /// Compare one declared counter with another.
    CounterVarCompare {
        left: String,
        op: AssertCompareOp,
        right: String,
    },
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
            let assertion = match parsed {
                ParsedAssert::StringNonEmpty { var } => RuntimeAssert::StringNonEmpty { var },
                ParsedAssert::CounterVarCompare { left, op, right } => {
                    RuntimeAssert::CounterVarCompare { left, op, right }
                }
                _ => return None,
            };
            Some(RuntimeInvariant {
                name: invariant.name.clone(),
                when: invariant.when.clone(),
                assertion,
                enforcement_version: RUNTIME_INVARIANT_ENFORCEMENT_VERSION,
            })
        })
        .collect()
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
        .filter(|invariant| !declarations.supports(&invariant.when, &invariant.assert))
        .map(|invariant| invariant.name.clone())
        .collect()
}

/// Return declared counter and boolean variables whose values are governed by
/// model-proved invariants rather than the runtime-enforced assertion subset.
///
/// Caller payloads must not mutate these variables directly. Their values are
/// changed only by modeled transition effects, keeping production execution
/// and replay within the state space proved by the verification backends.
pub fn model_protected_state_var_names(automaton: &Automaton) -> BTreeSet<String> {
    let declarations = SafetyDeclarations::from_automaton(automaton);
    let mut protected = BTreeSet::new();
    for invariant in &automaton.invariants {
        let Some(parsed) = parse_assert_expr(&invariant.assert) else {
            continue;
        };
        if !parsed.is_supported_safety_assertion(
            &declarations.bool_names,
            &declarations.counter_names,
            &declarations.string_names,
            &declarations.status_names,
        ) {
            continue;
        }
        collect_model_protected_names(&parsed, &mut protected);
    }
    protected
}

fn collect_model_protected_names(assertion: &ParsedAssert, protected: &mut BTreeSet<String>) {
    match assertion {
        ParsedAssert::CounterPositive { var }
        | ParsedAssert::CounterCompare { var, .. }
        | ParsedAssert::BoolRequired { var, .. } => {
            protected.insert(var.clone());
        }
        ParsedAssert::And(parts) | ParsedAssert::Or(parts) => {
            for part in parts {
                collect_model_protected_names(part, protected);
            }
        }
        ParsedAssert::Always
        | ParsedAssert::NoFurtherTransitions
        | ParsedAssert::OrderingConstraint { .. }
        | ParsedAssert::NeverState { .. }
        | ParsedAssert::CounterVarCompare { .. }
        | ParsedAssert::StringNonEmpty { .. } => {}
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
    counters: &BTreeMap<String, usize>,
    fields: &serde_json::Map<String, serde_json::Value>,
) -> bool {
    match assertion {
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
            match op {
                AssertCompareOp::Gt => left > right,
                AssertCompareOp::Gte => left >= right,
                AssertCompareOp::Lt => left < right,
                AssertCompareOp::Lte => left <= right,
                AssertCompareOp::Eq => left == right,
            }
        }
    }
}
