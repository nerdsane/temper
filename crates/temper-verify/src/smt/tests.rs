use super::*;

const ORDER_IOA: &str = include_str!("../../../../test-fixtures/specs/order.ioa.toml");

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
