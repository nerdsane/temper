use super::*;
use crate::automaton::parse_automaton;
use std::collections::BTreeMap;

fn parameter_counter_spec(assertion: &str) -> Automaton {
    parse_automaton(&format!(
        r#"
[automaton]
name = "Budget"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "budget"
type = "counter"
initial = "1"

[[action]]
name = "SetBudget"
kind = "input"
from = ["Active"]
to = "Active"
params = ["budget"]
effect = [{{ type = "set_counter_from_param", var = "budget", param = "budget" }}]

[[invariant]]
name = "BudgetInvariant"
when = ["Active"]
assert = "{assertion}"
"#
    ))
    .expect("parameter counter spec must parse")
}

fn evaluate_budget(assertion: &RuntimeAssert, budget: usize) -> bool {
    evaluate_runtime_assert(
        assertion,
        "Active",
        &BTreeMap::from([("budget".to_string(), budget)]),
        &BTreeMap::new(),
        &serde_json::Map::new(),
    )
}

fn guarded_terminal_spec(guard_from: &str) -> Automaton {
    parse_automaton(&format!(
        r#"
[automaton]
name = "GuardedBudget"
states = ["Active", "Done"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "budget"
type = "counter"
initial = "1"

[[action]]
name = "SetBudget"
kind = "input"
from = ["Active"]
to = "Active"
params = ["budget"]
effect = [{{ type = "set_counter_from_param", var = "budget", param = "budget" }}]

[[action]]
name = "Finish"
kind = "input"
from = ["Active"]
to = "Done"

[[action]]
name = "Guarded"
kind = "input"
from = ["{guard_from}"]
to = "{guard_from}"
guard = [{{ type = "min_count", var = "budget", min = 2 }}]

[[invariant]]
name = "DoneIsFinal"
when = ["Done"]
assert = "no_further_transitions"
"#
    ))
    .expect("guarded terminal spec must parse")
}

#[test]
fn parameter_counter_compounds_preserve_and_or_semantics() {
    let and_spec = parameter_counter_spec("budget <= 10 && budget > 0");
    let and_invariants = compile_runtime_invariants(&and_spec);
    assert_eq!(and_invariants.len(), 1);
    assert!(matches!(and_invariants[0].assertion, RuntimeAssert::And(_)));
    assert!(evaluate_budget(&and_invariants[0].assertion, 5));
    assert!(!evaluate_budget(&and_invariants[0].assertion, 0));
    assert!(!evaluate_budget(&and_invariants[0].assertion, 100));

    let or_spec = parameter_counter_spec("budget <= 10 || budget == 11");
    let or_invariants = compile_runtime_invariants(&or_spec);
    assert_eq!(or_invariants.len(), 1);
    assert!(matches!(or_invariants[0].assertion, RuntimeAssert::Or(_)));
    assert!(evaluate_budget(&or_invariants[0].assertion, 11));
    assert!(!evaluate_budget(&or_invariants[0].assertion, 100));
}

#[test]
fn parameter_counter_effect_rejects_terminal_assertion_without_runtime_equivalence() {
    let spec = parameter_counter_spec("budget <= 10 || no_further_transitions");
    assert_eq!(
        unsupported_safety_invariant_names(&spec),
        vec!["BudgetInvariant"]
    );
    assert!(compile_runtime_invariants(&spec).is_empty());
}

#[test]
fn parameter_counter_effect_preserves_unrelated_terminal_assertion() {
    let spec = parameter_counter_spec("no_further_transitions");
    assert!(unsupported_safety_invariant_names(&spec).is_empty());
    assert!(compile_runtime_invariants(&spec).is_empty());
}

#[test]
fn parameter_counter_guard_rejects_terminal_assertion_in_same_state() {
    let spec = guarded_terminal_spec("Done");
    assert_eq!(
        unsupported_safety_invariant_names(&spec),
        vec!["DoneIsFinal"]
    );
}

#[test]
fn parameter_counter_guard_preserves_terminal_assertion_in_unrelated_state() {
    let spec = guarded_terminal_spec("Active");
    assert!(unsupported_safety_invariant_names(&spec).is_empty());
}

#[test]
fn output_parameter_dependencies_do_not_affect_runtime_admission() {
    let mut spec = guarded_terminal_spec("Done");
    let guarded = spec
        .actions
        .iter_mut()
        .find(|action| action.name == "Guarded")
        .expect("guarded action exists");
    guarded.kind = "output".to_string();
    assert!(unsupported_safety_invariant_names(&spec).is_empty());
}

#[test]
fn tautological_or_does_not_depend_on_terminal_semantics() {
    let mut spec = guarded_terminal_spec("Done");
    spec.invariants[0].assert = "true || no_further_transitions".to_string();
    assert!(unsupported_safety_invariant_names(&spec).is_empty());

    spec.invariants[0].assert = "true && no_further_transitions".to_string();
    assert_eq!(
        unsupported_safety_invariant_names(&spec),
        vec!["DoneIsFinal"]
    );
}

#[test]
fn tautologies_do_not_compile_runtime_contracts() {
    let plain = parameter_counter_spec("true");
    assert!(compile_runtime_invariants(&plain).is_empty());

    let compound = parameter_counter_spec("true || budget <= 10");
    assert!(compile_runtime_invariants(&compound).is_empty());
}

#[test]
fn parameter_counter_guard_enforces_multihop_downstream_invariant() {
    let spec = parse_automaton(
        r#"
[automaton]
name = "MultihopBudget"
states = ["Active", "Unlocked", "Done"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "budget"
type = "counter"
initial = "1"

[[state]]
name = "safe"
type = "bool"
initial = "false"

[[action]]
name = "SetBudget"
kind = "input"
from = ["Active"]
to = "Active"
params = ["budget"]
effect = [{ type = "set_counter_from_param", var = "budget", param = "budget" }]

[[action]]
name = "Unlock"
kind = "input"
from = ["Active"]
to = "Unlocked"
guard = [{ type = "min_count", var = "budget", min = 2 }]

[[action]]
name = "Finish"
kind = "input"
from = ["Unlocked"]
to = "Done"

[[invariant]]
name = "DoneMustBeSafe"
when = ["Done"]
assert = "safe"
"#,
    )
    .expect("multi-hop guard spec must parse");

    assert!(unsupported_safety_invariant_names(&spec).is_empty());
    let runtime = compile_runtime_invariants(&spec);
    assert_eq!(runtime.len(), 1);
    assert!(matches!(
        runtime[0].assertion,
        RuntimeAssert::BoolRequired {
            ref var,
            expect: true
        } if var == "safe"
    ));
}

#[test]
fn parameter_counter_guard_rejects_multihop_mixed_terminal_compound() {
    let spec = parse_automaton(
        r#"
[automaton]
name = "MixedTerminal"
states = ["Active", "Unlocked", "Done"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "budget"
type = "counter"
initial = "1"

[[state]]
name = "safe"
type = "bool"
initial = "false"

[[action]]
name = "SetBudget"
kind = "input"
from = ["Active"]
to = "Active"
params = ["budget"]
effect = [{ type = "set_counter_from_param", var = "budget", param = "budget" }]

[[action]]
name = "Unlock"
kind = "input"
from = ["Active"]
to = "Unlocked"
guard = [{ type = "min_count", var = "budget", min = 2 }]

[[action]]
name = "Finish"
kind = "input"
from = ["Unlocked"]
to = "Done"

[[action]]
name = "Continue"
kind = "input"
from = ["Done"]
to = "Done"

[[invariant]]
name = "DoneSafety"
when = ["Done"]
assert = "safe && no_further_transitions"
"#,
    )
    .expect("mixed terminal compound spec must parse");

    assert_eq!(
        unsupported_safety_invariant_names(&spec),
        vec!["DoneSafety"]
    );
    assert!(compile_runtime_invariants(&spec).is_empty());
}
