//! A count parameter is explored as far as its value, after the move's own
//! arithmetic, lands inside the bounds of every counter it reaches and no
//! further than one past the end of every list position it gives.

use std::time::{Duration, Instant};

use stateright::Model;
use temper_verify::model::{ParamValue, build_model_from_ioa, semantics::successors};
use temper_verify::{CascadeLevel, TemperModelState, VerificationCascade};

/// A move from `On` to `Done` with `effects`, over counters `x`, `y` and `w`
/// that start at 0 and `z` that starts at 5, and the invariant `assert`.
/// The model holds only the counters some rule reads, so each `assert`
/// reads every counter its test looks at.
fn arithmetic(effects: &[&str], assert: &str) -> String {
    let effects = effects
        .iter()
        .map(|e| format!("\"{e}\""))
        .collect::<Vec<_>>()
        .join(", ");
    let counter = |name: &str, initial: u32| {
        format!("[[state]]\nname = \"{name}\"\ntype = \"counter\"\ninitial = {initial}\n")
    };
    format!(
        r#"
[automaton]
name = "Arithmetic"
states = ["On", "Done"]
initial = "On"

{}
{}
{}
{}
[[action]]
name = "Write"
from = ["On"]
to = "Done"
effect = [{effects}]

[[invariant]]
name = "Safe"
assert = "{assert}"
"#,
        counter("x", 0),
        counter("y", 0),
        counter("w", 0),
        counter("z", 5),
    )
}

fn model_check(spec: &str) -> temper_verify::LevelResult {
    VerificationCascade::from_ioa(spec)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run()
        .level_result(CascadeLevel::ModelCheck)
        .cloned()
        .expect("L1 ran")
}

/// The counterexample L1 found for `Safe`: the parameter `a` it chose, and
/// the last state.
fn broken_safe(l1: &temper_verify::LevelResult) -> (ParamValue, TemperModelState) {
    assert!(!l1.passed, "{}", l1.summary);
    let found = l1
        .verification
        .as_ref()
        .unwrap()
        .counterexamples
        .iter()
        .find(|c| c.violated.iter().any(|name| name == "Safe"))
        .unwrap_or_else(|| panic!("no counterexample for Safe: {}", l1.summary));
    let a = found
        .trace
        .iter()
        .find_map(|(_, action)| action.as_ref()?.params.get("a").cloned())
        .expect("the move's parameter");
    (a, found.trace.last().unwrap().0.clone())
}

/// A parameter is explored as far as its value can travel through the
/// move's arithmetic and still land inside the bounds: here `a` must reach
/// 4 for `x`, bounded at 2, to be 1.
#[test]
fn a_parameter_reaches_a_counter_through_a_subtraction() {
    let spec = arithmetic(&["y = params.a", "y -= 3", "x = y", "y = 0"], "x != 1");
    let (a, last) = broken_safe(&model_check(&spec));
    assert_eq!(a, ParamValue::Count(4));
    assert_eq!(last.counters["x"], 1);
}

#[test]
fn a_parameter_reaches_a_counter_through_an_addition() {
    let spec = arithmetic(&["y = params.a", "y += 2", "x = y", "y = 0"], "x != 4");
    let (a, last) = broken_safe(&model_check(&spec));
    assert_eq!(a, ParamValue::Count(2));
    assert_eq!(last.counters["x"], 4);
}

#[test]
fn a_parameter_reaches_a_counter_through_a_chain() {
    let spec = arithmetic(
        &[
            "y = params.a",
            "y -= 1",
            "w = y",
            "w -= 2",
            "x = w",
            "y = 0",
            "w = 0",
        ],
        "x != 2",
    );
    let (a, last) = broken_safe(&model_check(&spec));
    assert_eq!(a, ParamValue::Count(5));
    assert_eq!(last.counters["x"], 2);
}

/// A parameter can always take `x` past the bound it is explored to, so a
/// pass says it is bounded and names `x`.
#[test]
fn a_parameter_that_can_take_a_counter_past_its_bound_makes_a_bounded_pass() {
    let spec = arithmetic(
        &["y = params.a", "y -= 3", "x = y", "y = 0"],
        "y == 0 && x >= 0",
    );
    let l1 = model_check(&spec);
    assert!(l1.passed, "{}", l1.summary);
    assert!(l1.summary.contains("bounded"), "{}", l1.summary);
    let cut = &l1.verification.as_ref().unwrap().bounded_by;
    assert!(cut.iter().any(|(name, _)| name == "x"), "{cut:?}");
}

/// How far `params.a` can go with `x` inside its bound depends on
/// `params.b`, which nothing limits, so `a` is tried with `b` taken as 0 and
/// the pass says so.
#[test]
fn a_subtraction_that_cannot_be_worked_back_makes_a_bounded_pass() {
    let spec = arithmetic(&["x = params.a", "x -= params.b"], "y == 0 && x >= 0");
    let l1 = model_check(&spec);
    assert!(l1.passed, "{}", l1.summary);
    let cut = &l1.verification.as_ref().unwrap().bounded_by;
    assert!(cut.iter().any(|(name, _)| name == "params.a"), "{cut:?}");
}

/// `x = a; x -= a` leaves `x` at 0 whatever `a` is, so `a` goes as far as
/// `y` allows, and 5 takes `y` to 1. The rule reads `x`, always true, so the
/// model keeps it.
#[test]
fn a_parameter_that_cancels_itself_in_one_counter_reaches_another() {
    let spec = r#"
[automaton]
name = "Cancel"
states = ["On", "Done"]
initial = "On"

[[state]]
name = "x"
type = "counter"
initial = 0

[[state]]
name = "y"
type = "counter"
initial = 0

[[action]]
name = "Write"
from = ["On"]
to = "Done"
effect = ["x = params.a", "x -= params.a", "y = params.a", "y -= 4"]

[[invariant]]
name = "Safe"
assert = "y != 1 && x == 0"
"#;
    let (a, last) = broken_safe(&model_check(spec));
    assert_eq!(a, ParamValue::Count(5));
    assert_eq!(last.counters["x"], 0);
    assert_eq!(last.counters["y"], 1);
}

/// A value past `a`'s range takes both `x` and `y` past their bounds, and
/// the pass names both.
#[test]
fn a_bounded_pass_names_every_counter_a_parameter_can_take_past_its_bound() {
    let spec = arithmetic(
        &["x = params.a", "y = params.a"],
        "w == 0 && x >= 0 && y >= 0",
    );
    let l1 = model_check(&spec);
    assert!(l1.passed, "{}", l1.summary);
    let cut = &l1.verification.as_ref().unwrap().bounded_by;
    for counter in ["x", "y"] {
        assert!(
            cut.iter().any(|(name, _)| name == counter),
            "{counter}: {cut:?}"
        );
    }
}

/// `x += z; x -= z` leaves `x` at `a` whatever `z` holds, so `a` goes only
/// as far as `x`'s bound, here one past the 2 its rule compares it with,
/// even with `z` at 5000.
#[test]
fn adding_and_taking_away_the_same_counter_cancels() {
    let spec = arithmetic(&["x = params.a", "x += z", "x -= z"], "x != 2")
        .replace("initial = 5\n", "initial = 5000\n");
    let model = build_model_from_ioa(&spec, 2).unwrap();
    let initial = model.init_states().remove(0);
    let choices = successors(&model, &model.transitions[0], &initial);
    assert_eq!(choices.len(), 4, "a from 0 to 3");

    let (a, last) = broken_safe(&model_check(&spec));
    assert_eq!(a, ParamValue::Count(2));
    assert_eq!(last.counters["x"], 2);
    assert_eq!(last.counters["z"], 5000);
}

/// Each `cN -= 1` leaves the larger of `cN - 1` and 0, and adding twenty of
/// them to `x` does not make a range out of every way of choosing between
/// them.
#[test]
fn a_long_chain_of_counters_that_stop_at_zero_is_quick_to_range() {
    let counters = 20;
    let declared: String = (0..counters)
        .map(|i| format!("[[state]]\nname = \"c{i}\"\ntype = \"counter\"\ninitial = 1\n\n"))
        .collect();
    let effects: Vec<String> = std::iter::once("\"x = params.a\"".to_string())
        .chain((0..counters).flat_map(|i| [format!("\"c{i} -= 1\""), format!("\"x += c{i}\"")]))
        .collect();
    let spec = format!(
        r#"
[automaton]
name = "Chain"
states = ["On", "Done"]
initial = "On"

[[state]]
name = "x"
type = "counter"
initial = 0

{declared}[[action]]
name = "Sum"
from = ["On"]
to = "Done"
effect = [{}]

[[invariant]]
name = "Safe"
assert = "x >= 0"
"#,
        effects.join(", ")
    );
    let model = build_model_from_ioa(&spec, 2).unwrap();
    let initial = model.init_states().remove(0);
    let started = Instant::now();
    let choices = successors(&model, &model.transitions[0], &initial);
    let took = started.elapsed();
    assert!(
        choices
            .iter()
            .any(|(params, _)| params["a"] == ParamValue::Count(2)),
        "{choices:?}"
    );
    assert!(took < Duration::from_secs(2), "ranging took {took:?}");
}
