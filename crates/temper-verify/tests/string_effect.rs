use temper_verify::checker::check_model;
use temper_verify::model::build_model_from_ioa;
use temper_verify::{CascadeLevel, VerificationCascade};

const SPEC: &str = include_str!("../../../test-fixtures/specs/string_effect.ioa.toml");

#[test]
fn string_clear_passes_every_cascade_level() {
    let result = VerificationCascade::from_ioa(SPEC)
        .with_sim_seeds(20)
        .with_prop_test_cases(100)
        .run();
    assert!(result.all_passed, "{result:#?}");
    for level in [
        CascadeLevel::SymbolicVerification,
        CascadeLevel::ModelCheck,
        CascadeLevel::Simulation,
        CascadeLevel::PropertyTest,
    ] {
        assert!(result.level_result(level).unwrap().passed);
    }
}

#[test]
fn invariant_needs_the_string_clear() {
    for broken in [
        SPEC.replace("\"error = ''\", ", ""),
        SPEC.replace("error = ''", "error = 'stale'"),
    ] {
        let result = check_model(&build_model_from_ioa(&broken, 2).unwrap());
        assert!(!result.all_properties_hold);
        assert!(
            result
                .counterexamples
                .iter()
                .any(|c| c.property == "Invariants"
                    && c.trace
                        .last()
                        .is_some_and(|(state, _)| !state.strings["error"].is_empty())),
            "{result:#?}"
        );
    }
}

#[test]
fn effect_only_strings_have_only_initial_and_literal_values_and_guards_see_writes() {
    let source = r#"
[automaton]
name = "FiniteStrings"
states = ["Running"]
initial = "Running"
allow_indefinite_states = ["Running"]

[[state]]
name = "message"
type = "string"
initial = "initial"

[[action]]
name = "Write"
from = ["Running"]
to = "Running"
effect = ["message = 'intermediate'", "message = 'written'"]

[[action]]
name = "Clear"
from = ["Running"]
to = "Running"
guard = "message == 'written'"
effect = ["message = ''"]
"#;
    let model = build_model_from_ioa(source, 2).unwrap();
    let result = check_model(&model);
    assert!(
        result.all_properties_hold && result.is_complete,
        "{result:#?}"
    );
    assert!(result.dead_transitions.is_empty(), "{result:#?}");
    // Initial, final literal, empty. The intermediate write is never a state,
    // and effect-only fields acquire no arbitrary parameter equality symbols.
    assert_eq!(result.states_explored, 3);

    let broken = source.replace("message == 'written'", "message == 'intermediate'");
    let result = check_model(&build_model_from_ioa(&broken, 2).unwrap());
    assert_eq!(result.dead_transitions, ["Clear [Running -> Running]"]);
}

#[test]
fn smt_observes_the_literal_and_reports_a_stale_assignment() {
    let broken = SPEC.replace("error = ''", "error = 'stale'");
    let result = temper_verify::smt::verify_symbolic(&broken, 2);
    assert!(
        result
            .not_proved
            .iter()
            .any(|(name, why)| name == "RecoveredClearsError"
                && *why == temper_verify::smt::NotProved::BrokenBy("Recover".into())),
        "{result:#?}"
    );
    assert!(
        result
            .inductive_invariants
            .iter()
            .any(|(name, passed)| name == "RecoveredClearsError" && !passed)
    );
}

#[test]
fn a_parameter_can_equal_a_literal_written_by_a_later_effect() {
    let source = r#"
[automaton]
name = "FutureLiteral"
states = ["Start", "Input", "Written", "Equal"]
initial = "Start"
terminal = ["Equal"]
[[state]]
name = "input"
type = "string"
initial = ""
[[state]]
name = "literal"
type = "string"
initial = ""
[[action]]
name = "Input"
from = ["Start"]
to = "Input"
params = ["input"]
[[action]]
name = "Write"
from = ["Input"]
to = "Written"
effect = ["literal = 'future'"]
[[action]]
name = "Compare"
from = ["Written"]
to = "Equal"
guard = "input == literal"
[[invariant]]
name = "NeverEqual"
assert = "status != 'Equal'"
"#;
    let result = check_model(&build_model_from_ioa(source, 2).unwrap());
    assert!(!result.all_properties_hold);
    let trace = &result.counterexamples[0].trace;
    let last = &trace.last().unwrap().0;
    assert_eq!(
        last.strings["input"],
        temper_verify::model::ModelString::from("future")
    );
}

#[test]
fn string_effects_keep_existing_parameter_constraints_in_the_cascade() {
    let source = SPEC.replace(
        "params = [\"error\"]",
        r#"params = ["error"]
[[action.constraints]]
kind = "param_nonempty"
param = "error"
[[action.constraints]]
kind = "param_equals_field"
param = "error"
field = "reason"
"#,
    ) + r#"
[[invariant]]
name = "FailureExplained"
assert = "status in ['Failed'] => !empty(error) && error == reason"
"#;
    let result = VerificationCascade::from_ioa(&source)
        .with_sim_seeds(10)
        .with_prop_test_cases(50)
        .run();
    assert!(result.all_passed, "{result:#?}");
}

#[test]
fn string_parameters_override_effects_in_every_cascade_level() {
    let source = include_str!("../../../test-fixtures/specs/string_effect_param_order.ioa.toml");
    let result = VerificationCascade::from_ioa(source)
        .with_sim_seeds(20)
        .with_prop_test_cases(100)
        .run();
    assert!(result.all_passed, "{result:#?}");
    for level in [
        CascadeLevel::SymbolicVerification,
        CascadeLevel::ModelCheck,
        CascadeLevel::Simulation,
        CascadeLevel::PropertyTest,
    ] {
        assert!(result.level_result(level).unwrap().passed);
    }
}

const SEQUENTIAL_PARAMS: &str = r#"
[automaton]
name = "SequentialParams"
states = ["Start", "First", "Second", "Equal"]
initial = "Start"
terminal = ["Equal"]
[[state]]
name = "x"
type = "string"
initial = ""
[[state]]
name = "y"
type = "string"
initial = ""
[[action]]
name = "A"
from = ["Start"]
to = "First"
params = ["x"]
[[action]]
name = "B"
from = ["First"]
to = "Second"
params = ["y"]
[[action]]
name = "C"
from = ["Second"]
to = "Equal"
guard = "x == y && y == 'special'"
"#;

#[test]
fn sequential_string_parameters_share_comparison_literals() {
    for comparison in ["y == 'special'", "'special' == y", "y in ['special']"] {
        let source = SEQUENTIAL_PARAMS.replace("y == 'special'", comparison);
        let result = check_model(&build_model_from_ioa(&source, 2).unwrap());
        assert!(
            result.all_properties_hold && result.is_complete,
            "{result:#?}"
        );
        assert!(
            result.dead_transitions.is_empty(),
            "{comparison}: {result:#?}"
        );

        // An invariant that reads x keeps x, and the y it is compared with,
        // tracked: Equal is reachable only with both 'special'.
        let unsafe_source = source
            + r#"
[[invariant]]
name = "EqualOnlyEmpty"
assert = "status != 'Equal' || empty(x)"
"#;
        assert_sequential_special_counterexample(&unsafe_source);
    }
}

#[test]
fn sequential_string_parameters_share_invariant_literals() {
    let source = SEQUENTIAL_PARAMS.replace("x == y && y == 'special'", "x == y")
        + r#"
[[invariant]]
name = "NeverEqualSpecial"
assert = "status != 'Equal' || y != 'special'"
"#;
    assert_sequential_special_counterexample(&source);
}

/// A guard that reads only strings parameters write leaves them unmodeled, as
/// before string effects, so it adds no states to check.
#[test]
fn a_guard_on_parameter_written_strings_adds_no_states() {
    let spec = |guard: &str| {
        format!(
            r#"
[automaton]
name = "Asked"
states = ["Draft", "Ready", "Done"]
initial = "Draft"
terminal = ["Done"]
[[state]]
name = "source"
type = "string"
initial = ""
[[state]]
name = "asked"
type = "string"
initial = ""
[[state]]
name = "tries"
type = "counter"
initial = 0
[[action]]
name = "Describe"
from = ["Draft", "Ready"]
to = "Ready"
params = ["source", "asked"]
guard = "tries < 3"
effect = ["tries += 1"]
[[action]]
name = "Finish"
from = ["Ready"]
to = "Done"
{guard}
"#
        )
    };
    let explored =
        |source: &str| check_model(&build_model_from_ioa(source, 2).unwrap()).states_explored;
    assert_eq!(
        explored(&spec("guard = \"source != 'asked' || asked != ''\"")),
        explored(&spec(""))
    );
}

/// A string never compared with the status takes no place name as a choice:
/// to it a place is no different from any other value, so declaring more
/// places adds no states.
#[test]
fn places_add_no_choices_to_a_string_never_compared_with_status() {
    let spec = |places: &str| {
        format!(
            r#"
[automaton]
name = "Release"
states = ["Draft", "Live"{places}]
initial = "Draft"
[[state]]
name = "released"
type = "string"
initial = ""
[[state]]
name = "tries"
type = "counter"
initial = 0
[[action]]
name = "GoLive"
from = ["Draft", "Live"]
to = "Live"
params = ["released"]
guard = "tries < 3"
effect = ["tries += 1"]
[[invariant]]
name = "DraftHasNoRelease"
assert = "status in ['Draft'] => released == ''"
"#
        )
    };
    let explored =
        |source: &str| check_model(&build_model_from_ioa(source, 2).unwrap()).states_explored;
    assert_eq!(
        explored(&spec(r#", "Paused", "Stuck", "Done""#)),
        explored(&spec(""))
    );
}

/// A string compared with the status still takes every place name as a
/// choice: a move can save `'Live'` and the record then go `Live`.
#[test]
fn a_string_compared_with_status_takes_place_names() {
    let source = r#"
[automaton]
name = "Release"
states = ["Draft", "Live"]
initial = "Draft"
[[state]]
name = "released"
type = "string"
initial = ""
[[action]]
name = "GoLive"
from = ["Draft"]
to = "Live"
params = ["released"]
[[invariant]]
name = "ReleaseIsNotAPlace"
assert = "released != status"
"#;
    let result = check_model(&build_model_from_ioa(source, 2).unwrap());
    assert!(
        result.counterexamples.iter().any(|example| {
            example.property == "Invariants"
                && example.trace.last().is_some_and(|(state, _)| {
                    state.strings["released"] == temper_verify::model::ModelString::from("Live")
                })
        }),
        "{result:#?}"
    );
}

fn assert_sequential_special_counterexample(source: &str) {
    let result = check_model(&build_model_from_ioa(source, 2).unwrap());
    assert!(!result.all_properties_hold, "{result:#?}");
    assert!(
        result.counterexamples.iter().any(|example| {
            example.property == "Invariants"
                && example.trace.last().is_some_and(|(state, _)| {
                    state.status == "Equal"
                        && state.strings["x"] == temper_verify::model::ModelString::from("special")
                        && state.strings["y"] == temper_verify::model::ModelString::from("special")
                })
        }),
        "{result:#?}"
    );
}
