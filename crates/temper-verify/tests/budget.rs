//! A budgeted cascade answers in bounded time, however large the spec's state
//! space, and says which type ran out and why.

use std::time::{Duration, Instant};

use temper_verify::{Budget, CascadeLevel, Clock, VerificationCascade};

/// A spec whose model check runs for minutes with no budget.
const HEAVY_FEED: &str = include_str!("../../../test-fixtures/budget/heavy_feed.toml");

const ORDER_IOA: &str = include_str!("../../../test-fixtures/specs/order.ioa.toml");

fn budget(time: Duration) -> Budget {
    Budget {
        time,
        states: 5_000_000,
    }
}

/// A clock that moves 10 us each time the cascade reads it: a 1 s budget runs
/// out after 100,000 readings, at the same point of the model check on any
/// machine, however fast or busy.
const STEPS: Clock = Clock::Steps(Duration::from_micros(10));

#[test]
fn a_spec_too_large_to_check_stops_at_its_budget_and_names_itself() {
    let result = VerificationCascade::from_ioa(HEAVY_FEED)
        .with_sim_seeds(5)
        .with_prop_test_cases(100)
        .with_budget(budget(Duration::from_secs(1)))
        .with_clock(STEPS)
        .run();

    assert!(!result.all_passed);
    let stopped = result
        .stopped
        .as_deref()
        .expect("the cascade says it stopped");
    assert!(
        stopped.starts_with("HeavyFeed: L1 Model Check STOPPED: ran out of its 1 s budget"),
        "{stopped}"
    );
    assert!(
        stopped.contains("fewer of them or smaller bounds"),
        "{stopped}"
    );
    let last = result.levels.last().expect("levels");
    assert_eq!(
        last.level,
        CascadeLevel::ModelCheck,
        "the levels after the cut do not run"
    );
    assert!(!last.passed);
    let verification = last.verification.as_ref().expect("model check result");
    assert!(!verification.is_complete);
}

/// Six counters each stepped by its own move: a million states at bound 9,
/// each cheap to step from.
const WIDE: &str = r#"
state = [
  { name = "a", type = "counter", initial = 0 },
  { name = "b", type = "counter", initial = 0 },
  { name = "c", type = "counter", initial = 0 },
  { name = "d", type = "counter", initial = 0 },
  { name = "e", type = "counter", initial = 0 },
  { name = "f", type = "counter", initial = 0 },
]
action = [
  { name = "A", kind = "input", from = ["On"], effect = ["a += 1"] },
  { name = "B", kind = "input", from = ["On"], effect = ["b += 1"] },
  { name = "C", kind = "input", from = ["On"], effect = ["c += 1"] },
  { name = "D", kind = "input", from = ["On"], effect = ["d += 1"] },
  { name = "E", kind = "input", from = ["On"], effect = ["e += 1"] },
  { name = "F", kind = "input", from = ["On"], effect = ["f += 1"] },
]

[automaton]
name = "Wide"
states = ["On"]
initial = "On"

# The model holds only the fields some rule reads.
[[invariant]]
name = "Held"
assert = "a >= 0 && b >= 0 && c >= 0 && d >= 0 && e >= 0 && f >= 0"
"#;

#[test]
fn a_finding_before_the_cut_is_named_and_a_cut_walk_proves_no_liveness_false() {
    // `offered <= 1` is false as soon as Stepped adds 2, long before the budget
    // runs out. Rest now goes to Done, a state off the start, so `reaches Done`
    // is still open on every walk the cut ends: each queued state then looks
    // like the end of a walk that never got there.
    let spec = HEAVY_FEED
        .replace(
            r#"{ name = "Rest", kind = "input", from = ["Running"], to = "Resting" },"#,
            r#"{ name = "Rest", kind = "input", from = ["Running"], to = "Done" },
  { name = "Again", kind = "input", from = ["Done"], to = "Resting" },"#,
        )
        .replace(
            r#"states = ["Resting", "Running"]"#,
            r#"states = ["Resting", "Running", "Done"]"#,
        );
    assert!(spec.contains(r#"to = "Done""#) && spec.contains(r#""Done"]"#));
    let spec = format!(
        "{spec}\n[[invariant]]\nname = \"FewOffered\"\nassert = \"offered <= 1\"\n\n\
         [[liveness]]\nname = \"Settles\"\nfrom = [\"Running\"]\nreaches = [\"Done\"]\n"
    );
    let result = VerificationCascade::from_ioa(&spec)
        .with_budget(budget(Duration::from_secs(1)))
        .with_clock(STEPS)
        .run();
    let stopped = result
        .stopped
        .as_deref()
        .expect("the cascade says it stopped");
    assert!(
        stopped.contains("ran out of its 1 s budget") && stopped.contains("found Invariants false"),
        "{stopped}"
    );
    assert!(!stopped.contains("ReachesTerminal"), "{stopped}");
}

#[test]
fn a_state_limit_stops_the_model_check_however_much_time_is_left() {
    let result = VerificationCascade::from_ioa(WIDE)
        .with_max_items(9)
        .with_budget(Budget {
            time: Duration::from_secs(600),
            states: 1_000,
        })
        .run();
    let stopped = result
        .stopped
        .as_deref()
        .expect("the cascade says it stopped");
    assert!(
        stopped.starts_with(
            "Wide: L1 Model Check STOPPED: reached its limit of 1000 generated states"
        ),
        "{stopped}"
    );
    assert_eq!(result.levels.len(), 2, "L0, then the model check it cut");
}

#[test]
fn a_spec_within_its_budget_is_checked_as_without_one() {
    let unbudgeted = VerificationCascade::from_ioa(ORDER_IOA)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run();
    let budgeted = VerificationCascade::from_ioa(ORDER_IOA)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .with_budget(budget(Duration::from_secs(120)))
        .run();
    assert!(budgeted.all_passed && unbudgeted.all_passed);
    assert!(budgeted.stopped.is_none());
    let summaries = |r: &temper_verify::CascadeResult| -> Vec<String> {
        r.levels.iter().map(|l| l.summary.clone()).collect()
    };
    assert_eq!(summaries(&budgeted), summaries(&unbudgeted));
}

/// One move setting thirteen flags from its parameters: 2^13 = 8192 choices,
/// more than one step of the model check explores.
fn thirteen_flags() -> String {
    let flags: Vec<String> = (0..13).map(|i| format!("f{i}")).collect();
    let states: Vec<String> = flags
        .iter()
        .map(|f| format!("  {{ name = \"{f}\", type = \"bool\", initial = false }},"))
        .collect();
    let params: Vec<String> = flags.iter().map(|f| format!("\"{f}\"")).collect();
    // The model holds only the fields some rule reads.
    let read: Vec<String> = flags.iter().map(|f| format!("({f} || !{f})")).collect();
    let effects: Vec<String> = flags
        .iter()
        .map(|f| format!("\"{f} = params.{f}\""))
        .collect();
    format!(
        "state = [\n{}\n]\naction = [\n  {{ name = \"Set\", kind = \"input\", from = [\"On\"], params = [{}], effect = [{}] }},\n]\n\n[automaton]\nname = \"Flags\"\nstates = [\"On\"]\ninitial = \"On\"\n\n[[invariant]]\nname = \"Held\"\nassert = \"{}\"\n",
        states.join("\n"),
        params.join(", "),
        effects.join(", "),
        read.join(" && "),
    )
}

#[test]
fn a_move_with_too_many_parameter_choices_fails_its_check_instead_of_panicking() {
    let result = VerificationCascade::from_ioa(&thirteen_flags()).run();
    let stopped = result
        .stopped
        .as_deref()
        .expect("the cascade says it stopped");
    assert!(
        stopped.starts_with("Flags: L1 Model Check FAILED: Set takes 8192 combinations of parameter values, more than the 4096"),
        "{stopped}"
    );
    assert_eq!(result.levels.len(), 1, "no level steps the move");
}

#[test]
fn a_huge_counter_bound_is_counted_not_built() {
    // `x = 1000000000` raises the bound for parameters whose effects use x:
    // building those choices would allocate a billion values. The rule reads
    // x, so it is not left out of the model as unread.
    let spec = r#"
[automaton]
name = "Big"
states = ["On"]
initial = "On"

[[state]]
name = "x"
type = "counter"
initial = 0

[[action]]
name = "Fill"
kind = "input"
from = ["On"]
effect = ["x = 1000000000"]

[[action]]
name = "Bump"
kind = "input"
from = ["On"]
params = ["n"]
effect = ["x += params.n"]

# The model holds only the fields some rule reads.
[[invariant]]
name = "Held"
assert = "x >= 0"
"#;
    let started = Instant::now();
    let result = VerificationCascade::from_ioa(spec).run();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "answered in {:?}",
        started.elapsed()
    );
    let stopped = result
        .stopped
        .as_deref()
        .expect("the cascade says it stopped");
    assert!(
        stopped.contains("Bump takes 1000000001 combinations"),
        "{stopped}"
    );
}

#[test]
fn work_exhaustion_after_l1_stops_the_affected_level() {
    let spec = r#"
[automaton]
name = "Strings"
states = ["On"]
initial = "On"

[[state]]
name = "value"
type = "string"
initial = ""

[[action]]
name = "Write"
kind = "input"
from = ["On"]
to = "On"
params = [{ name = "value", type = "string" }]

[[invariant]]
name = "tracked"
assert = "value == value"
"#;
    // No simulation seeds leaves the work for L3. Both callers of ran_out
    // must notice exhaustion without relying on a model-check completion flag.
    for (seeds, states, level, count, label) in [
        (5, 1000, CascadeLevel::Simulation, 3, "L2 Simulation"),
        (0, 100, CascadeLevel::PropertyTest, 4, "L3 Property Tests"),
    ] {
        let started = Instant::now();
        let result = VerificationCascade::from_ioa(spec)
            .with_sim_seeds(seeds)
            .with_prop_test_cases(20)
            .with_budget(Budget {
                time: Duration::from_secs(600),
                states,
            })
            .run();
        assert!(started.elapsed() < Duration::from_secs(60));
        let l1 = &result.levels[1];
        assert_eq!(l1.level, CascadeLevel::ModelCheck);
        assert!(l1.passed, "{}", l1.summary);
        assert!(l1.verification.as_ref().unwrap().is_complete);
        assert!(!result.all_passed);
        assert_eq!(
            result.levels.len(),
            count,
            "no levels after the cut may run"
        );
        let last = result.levels.last().unwrap();
        assert_eq!(last.level, level);
        assert!(!last.passed);
        let stopped = result
            .stopped
            .as_deref()
            .expect("work exhaustion must stop the cascade");
        assert!(
            stopped.contains(&format!(
                "{label} STOPPED: reached its limit of {states} generated states or expansion steps"
            )),
            "{stopped}"
        );
    }
}
