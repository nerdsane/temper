use super::*;

const UNSUPPORTED_IOA: &str = r#"
[automaton]
name = "Workspace"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "used"
type = "counter"
initial = "0"

[[state]]
name = "quota"
type = "counter"
initial = "1"

[[invariant]]
name = "UnsupportedSafety"
when = ["Active"]
assert = "used ** quota"
"#;

#[test]
fn fallible_source_constructor_rejects_unsupported_safety() {
    let error = TransitionTable::try_from_ioa_source(UNSUPPORTED_IOA)
        .expect_err("unsupported safety must not produce an executable table");

    assert_eq!(error, "unsupported safety invariants: UnsupportedSafety");
}

#[test]
fn fallible_automaton_constructor_rejects_parameter_terminal_compound() {
    let source = r#"
[automaton]
name = "ParameterTerminal"
states = ["Active", "Done"]
initial = "Active"
allow_indefinite_states = ["Active", "Done"]

[[state]]
name = "budget"
type = "counter"
initial = "0"

[[state]]
name = "safe"
type = "bool"
initial = "false"

[[action]]
name = "SetBudget"
kind = "input"
from = ["Active"]
to = "Active"
params = ["amount"]
effect = [{ type = "set_counter_from_param", var = "budget", param = "amount" }]

[[action]]
name = "Finish"
kind = "input"
from = ["Active"]
to = "Done"
guard = [{ type = "min_count", var = "budget", min = 2 }]

[[invariant]]
name = "DoneSafety"
when = ["Done"]
assert = "safe && no_further_transitions"
"#;
    let automaton = temper_spec::automaton::parse_automaton(source).expect("parse fixture");

    let error = TransitionTable::try_from_automaton(&automaton)
        .expect_err("parameter terminal compound must fail closed");
    assert_eq!(error, "unsupported safety invariants: DoneSafety");
}

#[test]
#[should_panic(expected = "unsupported safety invariants: UnsupportedSafety")]
fn infallible_constructor_panics_instead_of_dropping_unsupported_safety() {
    TransitionTable::from_ioa_source(UNSUPPORTED_IOA);
}
