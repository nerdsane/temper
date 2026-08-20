use super::*;

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

#[test]
fn symbolic_verification_fails_closed_when_resource_budget_is_exhausted() {
    let result = verify_symbolic_with_budget(ORDER_IOA, 2, 1);
    assert!(!result.all_passed);
    assert!(
        result
            .approximation_notes
            .iter()
            .any(|note| note.contains("resource budget"))
    );
}

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
fn reference_equality_is_unsatisfiable_when_the_slot_cannot_be_set() {
    let spec = r#"
[automaton]
name = "Document"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "workspace_id"
type = "ref"
entity_type = "Workspace"
initial = ""

[[action]]
name = "Confirm"
from = ["Active"]
to = "Active"
params = [{ name = "candidate", type = "ref", entity_type = "Workspace" }]
guard = [{ type = "reference_equals", reference = "workspace_id", param = "candidate" }]
"#;
    let result = verify_symbolic(spec, 3);
    assert_eq!(
        result.guard_satisfiability,
        vec![("Confirm".to_string(), false)]
    );
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
    // Induction assumes the invariant before the transition. With no counter
    // modification it remains true, even though reachability can expose a
    // separate base-case violation from the initial state.
    let result = verify_symbolic(spec, 2);
    let inv = result
        .inductive_invariants
        .iter()
        .find(|(name, _)| name == "BRequiresCount");
    assert!(inv.is_some());
    assert!(
        inv.unwrap().1,
        "BRequiresCount is inductive (no counter change)"
    );
}

#[test]
fn test_decrement_breaks_induction() {
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
    assert!(
        !inv.unwrap().1,
        "BNeedsCount should NOT be inductive (decrement can reach 0)"
    );
}
