use temper_verify::{VerificationCascade, runtime_enforcement_warnings_from_ioa};

#[test]
fn parameter_controlled_terminal_compound_fails_cascade_preflight() {
    let spec = r#"
[automaton]
name = "ParameterTerminal"
states = ["Active", "Unlocked", "Done"]
initial = "Active"
allow_indefinite_states = ["Active", "Unlocked", "Done"]

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
"#;

    let result = VerificationCascade::from_ioa(spec)
        .with_sim_seeds(0)
        .with_prop_test_cases(0)
        .run();

    assert!(!result.all_passed);
    assert!(
        result.levels.is_empty(),
        "capability failure must precede proof"
    );
    assert_eq!(result.errors.len(), 1);
    assert_eq!(result.errors[0].code, "TVE001");
    assert_eq!(result.errors[0].invariant, "DoneSafety");
    assert_eq!(result.errors[0].assertion, "safe && no_further_transitions");
}

#[test]
fn parameter_derived_runtime_contract_is_disclosed_for_fresh_and_cached_paths() {
    let spec = r#"
[automaton]
name = "Usage"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "used"
type = "counter"
initial = "0"

[[action]]
name = "SetUsage"
kind = "input"
from = ["Active"]
to = "Active"
params = ["amount"]
effect = [{ type = "set_counter_from_param", var = "used", param = "amount" }]

[[invariant]]
name = "WithinLimit"
when = ["Active"]
assert = "used <= 5"
"#;

    let cached_warnings =
        runtime_enforcement_warnings_from_ioa(spec).expect("cached disclosure preflight");
    assert!(cached_warnings.iter().any(|warning| {
        warning.contains("WithinLimit") && warning.contains("not model-proved")
    }));

    let result = VerificationCascade::from_ioa(spec)
        .with_sim_seeds(0)
        .with_prop_test_cases(0)
        .run();
    assert!(
        result.all_passed,
        "runtime-enforced limit remains deployable"
    );
    assert!(result.warnings.iter().any(|warning| {
        warning.contains("WithinLimit") && warning.contains("not model-proved")
    }));
}
