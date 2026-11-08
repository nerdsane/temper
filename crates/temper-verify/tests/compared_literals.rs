//! A counter, or a list's length, is explored at least one past every
//! literal a guard or invariant compares it with.
//!
//! The model check explores each counter and list up to a bound. A rule
//! whose threshold sits above that bound was never reached: a count that
//! passes it was reported as holding, and a move that waits for it as dead.

use std::time::{Duration, Instant};

use proptest::prelude::*;
use temper_verify::model::build_model_from_ioa;
use temper_verify::{
    CascadeLevel, TemperModelState, VerificationCascade, check_model, verify_symbolic,
};

/// A batch's age counts up by the hour and must stay under 4.
const COUNT_UP: &str = r#"
[automaton]
name = "PearlBatch"
states = ["Fresh", "Served"]
initial = "Fresh"

[[state]]
name = "age_hours"
type = "counter"
initial = 0

[[action]]
name = "HourPasses"
from = ["Fresh"]
effect = ["age_hours += 1"]

[[action]]
name = "Serve"
from = ["Fresh"]
to = "Served"

[[invariant]]
name = "NoStalePearls"
assert = "age_hours < 4"
"#;

/// A card earns its reward at its fifth stamp.
const PUNCH_CARD: &str = r#"
[automaton]
name = "Punchcard"
states = ["Collecting", "Rewarded"]
initial = "Collecting"

[[state]]
name = "stamps"
type = "counter"
initial = 0

[[action]]
name = "Stamp"
from = ["Collecting"]
effect = ["stamps += 1"]

[[action]]
name = "Reward"
from = ["Collecting"]
to = "Rewarded"
guard = "stamps >= 5"
"#;

/// A tray holds cups, and must hold fewer than 4.
const CUP_TRAY: &str = r#"
[automaton]
name = "CupTray"
states = ["Filling", "Sealed"]
initial = "Filling"

[[state]]
name = "cups"
type = "list"
initial = []

[[action]]
name = "AddCup"
from = ["Filling"]
effect = ["append(cups, 'taro')"]

[[action]]
name = "Seal"
from = ["Filling"]
to = "Sealed"

[[invariant]]
name = "TrayFits"
assert = "len(cups) < 4"
"#;

/// A batch ships once it holds 5 cups.
const BATCH: &str = r#"
[automaton]
name = "Batch"
states = ["Packing", "Shipped"]
initial = "Packing"

[[state]]
name = "cups"
type = "list"
initial = []

[[action]]
name = "Pack"
from = ["Packing"]
effect = ["append(cups, 'taro')"]

[[action]]
name = "Ship"
from = ["Packing"]
to = "Shipped"
guard = "len(cups) >= 5"
"#;

fn cascade(spec: &str) -> temper_verify::CascadeResult {
    VerificationCascade::from_ioa(spec)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run()
}

#[test]
fn a_count_that_passes_its_threshold_breaks_the_rule() {
    let result = cascade(COUNT_UP);
    let l1 = result
        .level_result(CascadeLevel::ModelCheck)
        .expect("L1 ran");
    assert!(!l1.passed, "{}", l1.summary);
    let found = l1
        .verification
        .as_ref()
        .expect("L1 result")
        .counterexamples
        .iter()
        .find(|c| c.violated.iter().any(|name| name == "NoStalePearls"))
        .expect("a counterexample for NoStalePearls");
    let (last, _) = found.trace.last().expect("a trace");
    assert_eq!(last.counters.get("age_hours"), Some(&4), "{found:?}");
}

#[test]
fn a_move_that_waits_for_a_count_above_the_old_bound_can_fire() {
    let result = cascade(PUNCH_CARD);
    let l0 = result
        .level_result(CascadeLevel::SymbolicVerification)
        .expect("L0 ran");
    assert!(l0.passed, "{}", l0.summary);
    let l1 = result
        .level_result(CascadeLevel::ModelCheck)
        .expect("L1 ran");
    assert!(l1.passed, "{}", l1.summary);
    // Stamps can go past any bound, so L1 says how far it looked.
    assert!(l1.summary.contains("bounded"), "{}", l1.summary);
    assert!(
        !l1.summary.contains("all properties hold"),
        "{}",
        l1.summary
    );
}

#[test]
fn a_list_that_grows_past_its_threshold_breaks_the_rule() {
    let result = cascade(CUP_TRAY);
    let l1 = result
        .level_result(CascadeLevel::ModelCheck)
        .expect("L1 ran");
    assert!(!l1.passed, "{}", l1.summary);
    let found = l1
        .verification
        .as_ref()
        .expect("L1 result")
        .counterexamples
        .iter()
        .find(|c| c.violated.iter().any(|name| name == "TrayFits"))
        .expect("a counterexample for TrayFits");
    let (last, _) = found.trace.last().expect("a trace");
    assert_eq!(last.lists.get("cups").map(Vec::len), Some(4), "{found:?}");
}

#[test]
fn a_move_that_waits_for_a_list_longer_than_the_old_bound_can_fire() {
    let result = cascade(BATCH);
    let l0 = result
        .level_result(CascadeLevel::SymbolicVerification)
        .expect("L0 ran");
    assert!(l0.passed, "{}", l0.summary);
    let l1 = result
        .level_result(CascadeLevel::ModelCheck)
        .expect("L1 ran");
    assert!(l1.passed, "{}", l1.summary);
    assert!(l1.summary.contains("bounded"), "{}", l1.summary);
}

/// L0 reads a list only through its length and what it holds, so a length
/// compared with a large literal costs it no more than a small one.
#[test]
fn a_large_length_threshold_is_quick_for_the_symbolic_check() {
    let spec = BATCH.replace("len(cups) >= 5", "len(cups) >= 10000000");
    let started = Instant::now();
    let result = verify_symbolic(&spec, 2);
    let took = started.elapsed();
    let ship = result
        .guard_satisfiability
        .iter()
        .find(|(name, _)| name == "Ship")
        .map(|(_, sat)| *sat);
    assert_eq!(ship, Some(true), "{:?}", result.guard_satisfiability);
    assert!(took < Duration::from_secs(2), "L0 took {took:?}");
}

/// What a rule counts: a counter `x` that one move adds 1 to, or the
/// length of a list `items` that one move appends to.
#[derive(Clone, Copy, Debug)]
enum Measure {
    Counter,
    ListLength,
}

impl Measure {
    /// How a guard or invariant reads it.
    fn read(self) -> &'static str {
        match self {
            Measure::Counter => "x",
            Measure::ListLength => "len(items)",
        }
    }

    /// Its value in `state`.
    fn value(self, state: &TemperModelState) -> usize {
        match self {
            Measure::Counter => state.counters.get("x").copied().unwrap_or(0),
            Measure::ListLength => state.lists.get("items").map_or(0, Vec::len),
        }
    }

    /// A spec where one move adds one to it, with `guard` on the move when it
    /// is given, and the invariant `assert`.
    fn spec(self, guard: Option<&str>, assert: &str) -> String {
        let (declared, effect) = match self {
            Measure::Counter => ("name = \"x\"\ntype = \"counter\"\ninitial = 0", "x += 1"),
            Measure::ListLength => (
                "name = \"items\"\ntype = \"list\"\ninitial = []",
                "append(items, 'cup')",
            ),
        };
        let guard = guard
            .map(|g| format!("guard = \"{g}\"\n"))
            .unwrap_or_default();
        format!(
            r#"
[automaton]
name = "Tally"
states = ["Open"]
initial = "Open"

[[state]]
{declared}

[[action]]
name = "Add"
from = ["Open"]
{guard}effect = ["{effect}"]

[[invariant]]
name = "UnderThreshold"
assert = "{assert}"
"#
        )
    }
}

fn measure() -> impl Strategy<Value = Measure> {
    prop_oneof![Just(Measure::Counter), Just(Measure::ListLength)]
}

/// `m` compared with a threshold, written each way round, and the first
/// value of `m` that breaks it.
fn comparison(m: Measure) -> impl Strategy<Value = (String, usize)> {
    let v = m.read();
    (3usize..=24, 0usize..5).prop_map(move |(t, form)| match form {
        0 => (format!("{v} < {t}"), t),
        1 => (format!("{t} > {v}"), t),
        2 => (format!("{v} <= {t}"), t + 1),
        3 => (format!("{t} >= {v}"), t + 1),
        _ => {
            let allowed: Vec<String> = (0..t).map(|n| n.to_string()).collect();
            (format!("{v} in [{}]", allowed.join(", ")), t)
        }
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// A threshold above the default bound of 2 is still reached: the model
    /// check breaks the rule at the first value past it.
    #[test]
    fn a_threshold_above_the_default_bound_is_reached(
        (m, (assert, breaks_at)) in measure().prop_flat_map(|m| (Just(m), comparison(m)))
    ) {
        let spec = m.spec(None, &assert);
        let result = check_model(&build_model_from_ioa(&spec, 2).unwrap());
        let found = result
            .counterexamples
            .iter()
            .find(|c| c.property == "Invariants");
        prop_assert!(found.is_some(), "{assert} held: {result:?}");
        let (last, _) = found.unwrap().trace.last().unwrap();
        prop_assert_eq!(m.value(last), breaks_at);
    }

    /// A count the guard keeps at its threshold is checked at every value it
    /// can take, and the result is a proof, not a bounded pass.
    #[test]
    fn a_count_held_at_its_threshold_is_proved(m in measure(), threshold in 3usize..=24) {
        let v = m.read();
        let spec = m.spec(Some(&format!("{v} < {threshold}")), &format!("{v} <= {threshold}"));
        let result = cascade(&spec);
        let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
        prop_assert!(l1.passed, "{}", l1.summary);
        prop_assert!(l1.summary.contains("all properties hold"), "{}", l1.summary);
        let explored = l1.verification.as_ref().unwrap().states_explored;
        prop_assert!(explored > threshold, "{explored} states for {v} up to {threshold}");
    }
}
