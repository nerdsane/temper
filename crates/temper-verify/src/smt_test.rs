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
initial = []

[[action]]
name = "ConflictingContains"
from = ["S"]
to = "S"
guard = "'urgent' in labels && 'normal' in labels"
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
    // No count is both above 9 and below 5, so Z3 returns UNSAT whatever
    // the counter's bound.
    let spec = r#"
[automaton]
name = "DeadGuard"
states = ["A", "B"]
initial = "A"

[[state]]
name = "items"
type = "counter"
initial = 0

[[action]]
name = "Go"
from = ["A"]
to = "B"
guard = "items > 9 && items < 5"
"#;
    let result = verify_symbolic(spec, 2);
    let go_guard = result
        .guard_satisfiability
        .iter()
        .find(|(name, _)| name == "Go");
    assert!(go_guard.is_some());
    assert!(
        !go_guard.unwrap().1,
        "a guard no count satisfies should be unsatisfiable"
    );
}

fn not_proved(result: &SmtResult, name: &str) -> Option<NotProved> {
    let (_, proved) = result
        .inductive_invariants
        .iter()
        .find(|(n, _)| n == name)
        .unwrap_or_else(|| panic!("no result for {name}: {result:?}"));
    let why = result
        .not_proved
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, why)| why.clone());
    assert_eq!(*proved, why.is_none(), "{result:?}");
    why
}

#[test]
fn an_invariant_entered_from_a_state_where_it_fails_is_not_inductive() {
    // GoB fires from A with count = 0 and reaches B with count = 0, which
    // breaks the invariant: L1 finds it in one step. L0 used to assume
    // the invariant's body in the pre-state although A is outside its
    // trigger, and called it inductive.
    let spec = r#"
[automaton]
name = "NonInductive"
states = ["A", "B"]
initial = "A"

[[state]]
name = "count"
type = "counter"
initial = 0

[[action]]
name = "GoB"
from = ["A"]
to = "B"

[[invariant]]
name = "BRequiresCount"
assert = "status in ['B'] => count > 0"
"#;
    let result = verify_symbolic(spec, 2);
    assert_eq!(
        not_proved(&result, "BRequiresCount"),
        Some(NotProved::BrokenBy("GoB".to_string()))
    );
    assert!(result.all_passed, "a rule L0 cannot prove is left to L1");
}

/// The Getaway film's spec: `tickets_home <= 1` has no `status in [..]`
/// trigger, and `Bought` stays in its state, so L0 checked neither and
/// reported the rule inductive while L1 found a 23-move path that pays
/// for two flights home.
#[test]
fn a_rule_any_move_can_break_is_not_called_inductive() {
    let spec = include_str!("../../../test-fixtures/refused/trip_getaway.ioa.toml");
    let result = verify_symbolic(spec, 2);
    assert_eq!(
        not_proved(&result, "NeverPaysForTwoFlightsHome"),
        Some(NotProved::BrokenBy("Bought".to_string()))
    );
    assert_eq!(not_proved(&result, "TypeInvariant"), None);
}

#[test]
fn an_invariant_the_initial_state_breaks_is_not_proved() {
    let spec = r#"
[automaton]
name = "BadStart"
states = ["A"]
initial = "A"

[[state]]
name = "count"
type = "counter"
initial = 2

[[action]]
name = "Drain"
from = ["A"]
guard = "count > 0"
effect = ["count -= 1"]

[[invariant]]
name = "AtMostOne"
assert = "count <= 1"
"#;
    let result = verify_symbolic(spec, 2);
    assert_eq!(not_proved(&result, "AtMostOne"), Some(NotProved::Initial));
}

#[test]
fn list_membership_in_an_invariant_is_modeled() {
    // `'x' in tags` used to read as false wherever no guard tested 'x',
    // so the invariant was trivially "inductive".
    let spec = r#"
[automaton]
name = "Tags"
states = ["A"]
initial = "A"

[[state]]
name = "tags"
type = "list"
initial = []

[[action]]
name = "Tag"
from = ["A"]
effect = ["append(tags, params.tag)"]

[[invariant]]
name = "NeverX"
assert = "'x' not in tags"
"#;
    let result = verify_symbolic(spec, 2);
    assert_eq!(
        not_proved(&result, "NeverX"),
        Some(NotProved::BrokenBy("Tag".to_string()))
    );
}

#[test]
fn a_counter_named_like_a_list_length_is_its_own_value() {
    // The counter `tags_len` and list `tags`'s length were one solver
    // constant, so the counter was capped at the list bound and `Inc` could
    // never reach 4.
    let spec = r#"
[automaton]
name = "Collide"
states = ["Open"]
initial = "Open"

[[state]]
name = "tags"
type = "list"
initial = []

[[state]]
name = "tags_len"
type = "counter"
initial = 0

[[action]]
name = "Inc"
from = ["Open"]
effect = ["tags_len += 1"]

[[invariant]]
name = "NeverFour"
assert = "tags_len != 4"
"#;
    let result = verify_symbolic(spec, 2);
    assert_eq!(
        not_proved(&result, "NeverFour"),
        Some(NotProved::BrokenBy("Inc".to_string()))
    );
}

#[test]
fn a_guard_part_written_twice_is_two_unknowns() {
    // L1 reads `params.urgent == true` as unknown wherever it appears, so it
    // fires `Proceed`; L0 used to read both copies as one value, find the
    // guard false while `n == 0`, and prove `NeverReady`.
    let spec = r#"
[automaton]
name = "UnknownTwice"
states = ["Waiting", "Ready"]
initial = "Waiting"

[[state]]
name = "n"
type = "counter"
initial = 0

[[action]]
name = "Proceed"
from = ["Waiting"]
to = "Ready"
params = ["urgent"]
guard = "params.urgent == true && (!(params.urgent == true) || n > 0)"

[[invariant]]
name = "NIsZero"
assert = "n == 0"

[[invariant]]
name = "NeverReady"
assert = "status != 'Ready'"
"#;
    let result = verify_symbolic(spec, 2);
    assert_eq!(not_proved(&result, "NIsZero"), None);
    assert_eq!(
        not_proved(&result, "NeverReady"),
        Some(NotProved::BrokenBy("Proceed".to_string()))
    );
}

#[test]
fn invariants_are_proved_together() {
    // ClearA keeps `b => a` only where `b => c` holds (its guard is `!c`),
    // and ClearC keeps `b => c` only where `b => a` holds: neither is
    // inductive alone, both are together.
    let spec = r#"
[automaton]
name = "Together"
states = ["S"]
initial = "S"

[[state]]
name = "a"
type = "bool"
initial = false

[[state]]
name = "b"
type = "bool"
initial = false

[[state]]
name = "c"
type = "bool"
initial = false

[[action]]
name = "SetA"
from = ["S"]
effect = ["a = true"]

[[action]]
name = "SetC"
from = ["S"]
effect = ["c = true"]

[[action]]
name = "SetB"
from = ["S"]
guard = "a && c"
effect = ["b = true"]

[[action]]
name = "ClearA"
from = ["S"]
guard = "!c"
effect = ["a = false"]

[[action]]
name = "ClearC"
from = ["S"]
guard = "!a"
effect = ["c = false"]

[[invariant]]
name = "BNeedsA"
assert = "b => a"

[[invariant]]
name = "BNeedsC"
assert = "b => c"
"#;
    let result = verify_symbolic(spec, 2);
    assert_eq!(not_proved(&result, "BNeedsA"), None, "{result:?}");
    assert_eq!(not_proved(&result, "BNeedsC"), None, "{result:?}");
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
initial = 0

[[action]]
name = "GoB"
from = ["A"]
to = "B"
effect = ["count -= 1"]

[[invariant]]
name = "BNeedsCount"
assert = "status in ['B'] => count > 0"
"#;
    let result = verify_symbolic(spec, 2);
    let inv = result
        .inductive_invariants
        .iter()
        .find(|(name, _)| name == "BNeedsCount");
    assert!(inv.is_some());
    assert!(
        !inv.unwrap().1,
        "BNeedsCount should NOT be inductive (GoB reaches B with count 0)"
    );
}
