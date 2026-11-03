//! Count choices follow the input's comparisons, not unrelated large state.

use temper_verify::model::{ParamValue, build_model_from_ioa};
use temper_verify::{VerificationCascade, check_model};

fn source(state: &str, guard: &str, params: &str) -> String {
    format!(
        r#"
[automaton]
name = "Entry"
states = ["Outside", "Inside"]
initial = "Outside"
terminal = ["Inside"]
{state}
[[action]]
name = "Enter"
from = ["Outside"]
to = "Inside"
params = [{params}]
guard = "{guard}"
[[entry_rule]]
name = "entry"
place = "Inside"
assert = "true"
"#
    )
}

const P: &str = r#"{ name = "p", type = "counter" }"#;

#[test]
fn unrelated_large_counters_and_lists_do_not_stop_entry_verification() {
    let large_list = std::iter::repeat_n("'one'", 10000)
        .collect::<Vec<_>>()
        .join(",");
    for (state, read) in [
        (
            "[[state]]\nname = 'unrelated'\ntype = 'counter'\ninitial = 10000".into(),
            "unrelated >= 0",
        ),
        (
            format!("[[state]]\nname = 'unrelated'\ntype = 'list'\ninitial = [{large_list}]"),
            "len(unrelated) >= 0",
        ),
    ] {
        // Even reading the large value elsewhere in this guard does not
        // connect it to p's comparisons.
        for guard in ["params.p == 0".into(), format!("params.p == 0 && {read}")] {
            let source = source(&state, &guard, P);
            let result = VerificationCascade::from_ioa(&source)
                .with_sim_seeds(3)
                .with_prop_test_cases(50)
                .run();
            assert!(result.all_passed, "{guard}: {result:?}");
            let broken = source.replace("assert = \"true\"", "assert = \"false\"");
            let checked = check_model(&build_model_from_ioa(&broken, 2).unwrap());
            assert!(
                checked
                    .counterexamples
                    .iter()
                    .any(|c| c.property == "EntryRules"),
                "{checked:?}"
            );
        }
    }
}

#[test]
fn unrelated_literals_do_not_multiply_every_count_inputs_range() {
    let source = source(
        "",
        "params.p == 0 && params.q == 100",
        &format!("{P}, {{ name = 'q', type = 'counter' }}"),
    );
    let result = VerificationCascade::from_ioa(&source)
        .with_sim_seeds(3)
        .with_prop_test_cases(50)
        .run();
    assert!(result.all_passed, "{result:?}");
}

#[test]
fn count_comparisons_keep_transitive_parameter_and_state_values() {
    for (state, guard, params, expected) in [
        ("", "!(params.p not in [50])", P.into(), 50),
        (
            "",
            "params.q == 50 && params.p == params.q",
            format!("{P}, {{ name = 'q', type = 'counter' }}"),
            50,
        ),
        (
            "[[state]]\nname = 'required'\ntype = 'counter'\ninitial = 50",
            "params.p == required",
            P.into(),
            50,
        ),
        (
            "[[state]]\nname = 'items'\ntype = 'list'\ninitial = ['a','b','c','d']",
            "params.p == len(items)",
            P.into(),
            4,
        ),
    ] {
        let broken =
            source(state, guard, &params).replace("assert = \"true\"", "assert = \"false\"");
        let checked = check_model(&build_model_from_ioa(&broken, 2).unwrap());
        let found = checked
            .counterexamples
            .iter()
            .find(|c| c.property == "EntryRules")
            .expect("breaking input explored");
        assert_eq!(
            found.trace[0].1.as_ref().unwrap().params["p"],
            ParamValue::Count(expected)
        );
    }
}

/// The model holds only fields some rule reads, so `count` has one: its
/// bound is what limits `q`.
#[test]
fn effect_input_ranges_reach_compared_parameters() {
    let state = "[[state]]\nname = 'other'\ntype = 'counter'\ninitial = 50\n\
                 [[state]]\nname = 'count'\ntype = 'counter'\ninitial = 0\n\
                 [[invariant]]\nname = 'held'\nassert = 'count >= 0'";
    let source = source(
        state,
        "params.p > params.q",
        &format!("{P}, {{ name = 'q', type = 'counter' }}"),
    )
    .replace(
        "guard =",
        "effect = ['count = other', 'count -= params.q']\nguard =",
    )
    .replace("assert = \"true\"", "assert = \"false\"");
    let model = build_model_from_ioa(&source, 2).unwrap();
    let checked = check_model(&model);
    let found = checked
        .counterexamples
        .iter()
        .find(|c| c.property == "EntryRules")
        .expect("the compared input must reach the executable effect input");
    let action = found.trace[0].1.as_ref().unwrap();
    assert_eq!(action.params["p"], ParamValue::Count(49));
    assert_eq!(action.params["q"], ParamValue::Count(48));
}
