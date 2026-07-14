//! Runtime-enforced safety assertions shared by production and deterministic simulation.

use std::collections::BTreeMap;

use super::{AssertCompareOp, Automaton, ParsedAssert, parse_assert_expr};

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
    automaton
        .invariants
        .iter()
        .filter_map(|invariant| {
            let assertion = match parse_assert_expr(&invariant.assert)? {
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
