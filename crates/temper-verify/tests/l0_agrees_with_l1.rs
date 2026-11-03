//! L0 never calls an invariant inductive that the model check (L1) can break.
//!
//! L0 proves invariants by induction; L1 walks every reachable state. An
//! invariant that holds initially and that no transition can break from a
//! state where it holds is true in every reachable state, so a proof L1 can
//! contradict is a wrong proof. The Getaway film caught one: L0 reported
//! `tickets_home <= 1` inductive while L1 found a path to `tickets_home = 2`.

use proptest::prelude::*;
use temper_verify::model::build_model_from_ioa;
use temper_verify::{
    CascadeLevel, VerificationCascade, check_model, run_prop_tests_from_ioa, verify_symbolic,
};

const TRIP_REBOOKING: &str = include_str!("../../../test-fixtures/refused/trip_rebooking.ioa.toml");

/// The film's failure, through the cascade: L0 does not call the rule
/// inductive, and L1 names it and shows the whole path to the move that
/// breaks it.
#[test]
fn the_cascade_names_the_rule_l1_breaks_and_the_path_to_it() {
    let result = VerificationCascade::from_ioa(TRIP_REBOOKING)
        .with_sim_seeds(2)
        .with_prop_test_cases(20)
        .run();

    let l0 = result
        .level_result(CascadeLevel::SymbolicVerification)
        .expect("L0 ran");
    assert!(l0.passed, "{}", l0.summary);
    assert!(
        l0.summary.contains("1 of 2 invariants proved inductive")
            && l0
                .summary
                .contains("not proved, left to the model check: NeverPaysForTwoFlightsHome (Bought can break it)"),
        "{}",
        l0.summary
    );

    let l1 = result
        .level_result(CascadeLevel::ModelCheck)
        .expect("L1 ran");
    assert!(!l1.passed);
    let header = &l1.diagnostics[0];
    assert!(
        header.contains("(NeverPaysForTwoFlightsHome false)"),
        "{header}"
    );
    let path: Vec<&String> = l1
        .diagnostics
        .iter()
        .skip(1)
        .take_while(|line| line.starts_with("    "))
        .collect();
    let steps = l1.verification.as_ref().expect("L1 detail").counterexamples[0]
        .trace
        .len();
    assert_eq!(path.len(), steps, "every step is shown: {path:?}");
    assert!(path[0].contains("0. Away"), "{path:?}");
    assert!(
        path[path.len() - 2].contains("--Bought"),
        "the path ends at the move that breaks the rule: {path:?}"
    );
    assert!(path[path.len() - 1].contains("tickets_home=2"), "{path:?}");
}

/// The counterexample's structured trace ends at the first state that breaks
/// the rule, where it used to run on to the last such state Stateright
/// visited (here: on to `FlyHome` and `Home`).
#[test]
fn a_counterexample_ends_where_the_rule_breaks() {
    let model = build_model_from_ioa(TRIP_REBOOKING, 2).expect("spec builds");
    let result = check_model(&model);
    let violation = result
        .counterexamples
        .iter()
        .find(|c| c.property == "Invariants")
        .expect("L1 breaks the rule");
    assert_eq!(violation.violated, ["NeverPaysForTwoFlightsHome"]);

    let (last, after) = violation.trace.last().expect("a path");
    assert!(after.is_none());
    assert_eq!(last.counters["tickets_home"], 2);
    let (before, bought) = &violation.trace[violation.trace.len() - 2];
    assert_eq!(before.counters["tickets_home"], 1);
    assert_eq!(bought.as_ref().map(|a| a.name.as_str()), Some("Bought"));
    assert!(
        violation.trace[..violation.trace.len() - 1]
            .iter()
            .all(|(state, _)| state.counters["tickets_home"] <= 1),
        "every earlier state keeps the rule"
    );
}

/// L3's failure carries the same per-step trace as L1's, so an agent has a
/// whole path even when L1 stops on its budget before finding one.
#[test]
fn a_property_test_failure_carries_every_state_on_its_path() {
    // Mode 1 is deterministic: its walks are seeded by the case number.
    let result = run_prop_tests_from_ioa(TRIP_REBOOKING, 500, 30);
    let failure = result.failure.expect("a walk pays for two flights home");
    assert_eq!(failure.invariant, "NeverPaysForTwoFlightsHome");
    assert_eq!(failure.trace.len(), failure.action_sequence.len() + 1);
    let (last, after) = failure.trace.last().expect("a path");
    assert!(after.is_none());
    assert_eq!(last.counters["tickets_home"], 2);
    let (_, bought) = &failure.trace[failure.trace.len() - 2];
    assert_eq!(bought.as_ref().map(|a| a.name.as_str()), Some("Bought"));
}

/// Two rules for one action: the runtime takes the first whose source states
/// hold (temper-jit `evaluate_ctx`), and so does L1. It used to apply the
/// first rule's effects from every state, reaching `C` with `n = 2`.
#[test]
fn two_rules_for_one_action_step_as_the_runtime_takes_them() {
    let spec = r#"
[automaton]
name = "Dup"
states = ["A", "B", "C"]
initial = "A"

[[state]]
name = "n"
type = "counter"
initial = 0

[[action]]
name = "Go"
from = ["A"]
to = "B"
effect = ["n += 1"]

[[action]]
name = "Go"
from = ["B"]
to = "C"
effect = ["n = 0"]

[[invariant]]
name = "CleanInC"
assert = "status == 'C' => n == 0"
"#;
    let model = build_model_from_ioa(spec, 2).expect("spec builds");
    let checked = check_model(&model);
    assert!(checked.is_complete);
    assert!(
        checked.counterexamples.is_empty(),
        "{:?}",
        checked.counterexamples
    );
    assert!(
        checked.dead_transitions.is_empty(),
        "{:?}",
        checked.dead_transitions
    );
    let symbolic = verify_symbolic(spec, 2);
    assert!(
        symbolic
            .inductive_invariants
            .contains(&("CleanInC".to_string(), true))
    );
}

/// The simulator chooses an action in one state and may deliver it in
/// another. A late `Go` in `B` takes `B`'s rule whole, to `C` with `n = 5`;
/// it used to take `B`'s effects with `A`'s target and report `B` with
/// `n = 5`, which the runtime never reaches.
#[test]
fn a_late_action_takes_the_rule_of_the_state_it_arrives_in() {
    let spec = r#"
[automaton]
name = "LateGo"
states = ["A", "B", "C"]
initial = "A"

[[state]]
name = "n"
type = "counter"
initial = 0

[[action]]
name = "Go"
from = ["A"]
to = "B"
effect = ["n += 1"]

[[action]]
name = "Hop"
from = ["A"]
to = "B"

[[action]]
name = "Wait"
from = ["A", "B"]

[[action]]
name = "Go"
from = ["B"]
to = "C"
effect = ["n = 5"]

[[action]]
name = "Reset"
from = ["C"]
to = "A"
effect = ["n = 0"]

[[invariant]]
name = "BKeepsNSmall"
assert = "status == 'B' => n < 5"

[[invariant]]
name = "CHasFive"
assert = "status == 'C' => n == 5"
"#;
    let result = VerificationCascade::from_ioa(spec)
        .with_sim_seeds(5)
        .with_prop_test_cases(20)
        .run();
    let l2 = result
        .level_result(CascadeLevel::Simulation)
        .expect("L2 ran");
    assert!(l2.passed, "{}: {:?}", l2.summary, l2.diagnostics);
    assert!(result.all_passed);
}

// ---------------------------------------------------------------------------
// Generated specs
// ---------------------------------------------------------------------------

const STATES: [&str; 3] = ["S0", "S1", "S2"];

/// The counters: the second is named like the list's length, which L0 must
/// keep apart from it.
const COUNTERS: [&str; 2] = ["c0", "l0_len"];

fn counter_atom() -> impl Strategy<Value = String> {
    (
        prop::sample::select(COUNTERS.to_vec()),
        prop::sample::select(vec!["==", "!=", "<", "<=", ">", ">="]),
        0..=3i64,
    )
        .prop_map(|(c, op, n)| format!("{c} {op} {n}"))
}

fn bool_atom() -> impl Strategy<Value = String> {
    (0..2usize, any::<bool>()).prop_map(|(b, negated)| {
        if negated {
            format!("!b{b}")
        } else {
            format!("b{b}")
        }
    })
}

fn list_atom() -> impl Strategy<Value = String> {
    prop::sample::select(vec![
        "len(l0) <= 1",
        "len(l0) == 0",
        "'x' in l0",
        "'x' not in l0",
    ])
    .prop_map(str::to_string)
}

fn status_atom() -> impl Strategy<Value = String> {
    prop::sample::subsequence(STATES.to_vec(), 1..=2).prop_map(|states| {
        let quoted: Vec<String> = states.iter().map(|s| format!("'{s}'")).collect();
        format!("status in [{}]", quoted.join(", "))
    })
}

fn local_atom() -> impl Strategy<Value = String> {
    prop_oneof![counter_atom(), bool_atom(), list_atom()]
}

/// A guard atom: a condition on the record, or on a parameter L0 and L1
/// cannot read (the same text may appear twice in one guard).
fn guard_atom() -> impl Strategy<Value = String> {
    prop_oneof![
        3 => local_atom(),
        1 => prop::sample::select(vec!["params.p == true", "!(params.p == true)"])
            .prop_map(str::to_string),
    ]
}

/// A guard: a condition on counters, flags, the list and a parameter, or none.
fn guard() -> impl Strategy<Value = Option<String>> {
    prop::option::of(prop_oneof![
        guard_atom(),
        (guard_atom(), guard_atom()).prop_map(|(a, b)| format!("{a} && {b}")),
        (guard_atom(), guard_atom()).prop_map(|(a, b)| format!("{a} || {b}")),
        (guard_atom(), guard_atom(), guard_atom())
            .prop_map(|(a, b, c)| format!("{a} && ({b} || {c})")),
    ])
}

fn any_atom() -> impl Strategy<Value = String> {
    prop_oneof![local_atom(), status_atom()]
}

/// An invariant, with or without a status trigger.
fn invariant() -> impl Strategy<Value = String> {
    prop_oneof![
        any_atom(),
        (any_atom(), any_atom()).prop_map(|(a, b)| format!("{a} && {b}")),
        (any_atom(), any_atom()).prop_map(|(a, b)| format!("{a} || {b}")),
        (status_atom(), local_atom()).prop_map(|(s, a)| format!("{s} => {a}")),
    ]
}

/// One effect statement, as `(variable written, statement)`.
fn effect() -> impl Strategy<Value = (String, String)> {
    let counter = || prop::sample::select(COUNTERS.to_vec());
    prop_oneof![
        counter().prop_map(|c| (c.to_string(), format!("{c} += 1"))),
        counter().prop_map(|c| (c.to_string(), format!("{c} -= 1"))),
        (counter(), 0..=2i64).prop_map(|(c, n)| (c.to_string(), format!("{c} = {n}"))),
        (0..2usize, any::<bool>()).prop_map(|(b, v)| (format!("b{b}"), format!("b{b} = {v}"))),
        (0..2usize).prop_map(|b| (format!("b{b}"), format!("b{b} = b{}", 1 - b))),
        Just(("l0".to_string(), "append(l0, params.tag)".to_string())),
    ]
}

/// Effects writing distinct variables.
fn effects() -> impl Strategy<Value = Vec<String>> {
    prop::collection::vec(effect(), 0..=2).prop_map(|effects| {
        let mut written = Vec::new();
        let mut kept = Vec::new();
        for (var, statement) in effects {
            if !written.contains(&var) {
                written.push(var);
                kept.push(statement);
            }
        }
        kept
    })
}

fn action() -> impl Strategy<
    Value = (
        Vec<&'static str>,
        Option<&'static str>,
        Option<String>,
        Vec<String>,
    ),
> {
    (
        prop::sample::subsequence(STATES.to_vec(), 1..=3),
        prop::option::of(prop::sample::select(STATES.to_vec())),
        guard(),
        effects(),
    )
}

fn quoted(items: &[&str]) -> String {
    let items: Vec<String> = items.iter().map(|s| format!("\"{s}\"")).collect();
    format!("[{}]", items.join(", "))
}

/// A spec of up to five actions over three action names, so some names have
/// two rules (the runtime takes the first whose source states hold).
fn spec() -> impl Strategy<Value = String> {
    (
        prop::collection::vec(0..=1usize, 2),
        prop::collection::vec(any::<bool>(), 2),
        prop::collection::vec(action(), 1..=5),
        prop::collection::vec(invariant(), 1..=2),
    )
        .prop_map(|(counters, bools, actions, invariants)| {
            let mut toml = format!(
                "[automaton]\nname = \"Gen\"\nstates = {states}\ninitial = \"S0\"\nallow_indefinite_states = {states}\n",
                states = quoted(&STATES)
            );
            for (name, initial) in COUNTERS.iter().zip(&counters) {
                toml.push_str(&format!(
                    "\n[[state]]\nname = \"{name}\"\ntype = \"counter\"\ninitial = {initial}\n"
                ));
            }
            for (i, initial) in bools.iter().enumerate() {
                toml.push_str(&format!(
                    "\n[[state]]\nname = \"b{i}\"\ntype = \"bool\"\ninitial = {initial}\n"
                ));
            }
            toml.push_str("\n[[state]]\nname = \"l0\"\ntype = \"list\"\ninitial = []\n");
            for (i, (from, to, guard, effects)) in actions.iter().enumerate() {
                toml.push_str(&format!(
                    "\n[[action]]\nname = \"A{}\"\nfrom = {}\n",
                    i % 3,
                    quoted(from)
                ));
                if let Some(to) = to {
                    toml.push_str(&format!("to = \"{to}\"\n"));
                }
                let mut params = Vec::new();
                if guard.as_ref().is_some_and(|g| g.contains("params.p")) {
                    params.push("p");
                }
                if effects.iter().any(|e| e.contains("params.tag")) {
                    params.push("tag");
                }
                if !params.is_empty() {
                    toml.push_str(&format!("params = {}\n", quoted(&params)));
                }
                if let Some(guard) = guard {
                    toml.push_str(&format!("guard = \"{guard}\"\n"));
                }
                if !effects.is_empty() {
                    let effects: Vec<&str> = effects.iter().map(String::as_str).collect();
                    toml.push_str(&format!("effect = {}\n", quoted(&effects)));
                }
            }
            for (i, assert) in invariants.iter().enumerate() {
                toml.push_str(&format!(
                    "\n[[invariant]]\nname = \"I{i}\"\nassert = \"{assert}\"\n"
                ));
            }
            toml
        })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// For every invariant L0 proves, the model check of that invariant alone
    /// (so another invariant's violation cannot hide it) finds no violation.
    #[test]
    fn l0_never_proves_an_invariant_l1_breaks(spec in spec()) {
        let model = build_model_from_ioa(&spec, 2);
        prop_assert!(
            model.is_ok(),
            "generated spec does not build: {:?}\n{spec}",
            model.as_ref().err()
        );
        let model = model.unwrap();
        let symbolic = verify_symbolic(&spec, 2);
        for inv in &model.invariants {
            let proved = symbolic
                .inductive_invariants
                .iter()
                .any(|(name, proved)| name == &inv.name && *proved);
            if !proved {
                continue;
            }
            let mut alone = model.clone();
            alone.invariants.retain(|other| other.name == inv.name);
            let checked = check_model(&alone);
            prop_assert!(checked.is_complete);
            let broken = checked
                .counterexamples
                .iter()
                .find(|c| c.property == "Invariants");
            prop_assert!(
                broken.is_none(),
                "L0 proved {} but L1 breaks it: {:?}\n{spec}",
                inv.name,
                broken
            );
        }
    }
}
