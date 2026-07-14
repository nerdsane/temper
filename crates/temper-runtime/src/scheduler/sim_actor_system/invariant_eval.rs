//! Spec-derived invariant evaluation for actor simulation.

use super::super::sim_handler::{CompareOp, SimActorHandler, SpecAssert};

/// Evaluate a [`SpecAssert`] against handler state. Returns `true` if the
/// assertion holds, `false` if violated. Recurses through `And`/`Or`.
pub(super) fn evaluate_spec_assert(
    assert: &SpecAssert,
    handler: &dyn SimActorHandler,
    when: &[String],
    status_before: &str,
    status_after: &str,
    item_count: usize,
) -> bool {
    match assert {
        SpecAssert::CounterPositive { var } => {
            if var == "items" {
                item_count > 0
            } else {
                true
            }
        }
        SpecAssert::NoFurtherTransitions => !when.iter().any(|state| state == status_before),
        SpecAssert::OrderingConstraint { before, after } => {
            if status_after == after.as_str() {
                let events = handler.events_json();
                events.as_array().is_none_or(|events| {
                    events.iter().any(|event| {
                        event.get("to_status").and_then(|status| status.as_str())
                            == Some(before.as_str())
                    })
                })
            } else {
                true
            }
        }
        SpecAssert::NeverState { state } => status_after != state.as_str(),
        SpecAssert::CounterCompare { var, op, value } => {
            let counter_value = if var == "items" { item_count } else { 0 };
            match op {
                CompareOp::Gt => counter_value > *value,
                CompareOp::Gte => counter_value >= *value,
                CompareOp::Lt => counter_value < *value,
                CompareOp::Lte => counter_value <= *value,
                CompareOp::Eq => counter_value == *value,
            }
        }
        SpecAssert::BoolRequired { var, expect } => {
            handler.bool_field(var).unwrap_or(false) == *expect
        }
        SpecAssert::And(parts) => parts.iter().all(|part| {
            evaluate_spec_assert(part, handler, when, status_before, status_after, item_count)
        }),
        SpecAssert::Or(parts) => parts.iter().any(|part| {
            evaluate_spec_assert(part, handler, when, status_before, status_after, item_count)
        }),
    }
}
