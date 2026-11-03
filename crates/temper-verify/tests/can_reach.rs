//! A `[[liveness]]` rule with `can_reach` says a record in its `from` places
//! can always still reach one of the places it names: some moves lead there,
//! however the record got where it is. L1 refuses a record that never can,
//! with the path to it.
//!
//! Unlike `reaches`, the rule does not ask that every path get there, only
//! that the way is never closed off.

use std::time::Duration;

use temper_spec::automaton::{LivenessEnforcement, parse_automaton_with_liveness};
use temper_verify::{Budget, CascadeLevel, LevelResult, VerificationCascade};

/// A cup made by one barista. A wrong drink goes back to the line, but
/// still counts its barista, so no barista may claim it again.
const CUP: &str = r#"
[automaton]
name = "Cup"
states = ["Unmade", "Waiting", "BeingMade", "Made", "Refunded"]
initial = "Unmade"

[[state]]
name = "ticket"
type = "string"
initial = ""

[[state]]
name = "topping"
type = "string"
initial = ""

[[state]]
name = "makers"
type = "counter"
initial = 0

[[action]]
name = "Add"
from = ["Unmade"]
to = "Waiting"

[[action]]
name = "Claim"
from = ["Waiting"]
to = "BeingMade"
guard = "makers == 0 && Ticket[ticket].status == 'Paid' && Topping[topping].status == 'Stocked'"
effect = ["makers += 1"]

[[action]]
name = "Finish"
from = ["BeingMade"]
to = "Made"

[[action]]
name = "RefundCup"
from = ["Waiting"]
to = "Refunded"

[[action]]
name = "WrongDrink"
from = ["Made"]
to = "Waiting"

[[invariant]]
name = "one_barista_per_cup"
assert = "makers <= 1"
"#;

/// A cup on the line can always still be made.
const CAN_BE_MADE: &str = r#"
[[liveness]]
name = "can_still_be_made"
from = ["Waiting"]
can_reach = ["Made"]
"#;

const TICKET: &str = include_str!("../../../test-fixtures/specs/ticket.ioa.toml");

fn model_check(spec: &str) -> LevelResult {
    VerificationCascade::from_ioa(spec)
        .with_sim_seeds(2)
        .with_prop_test_cases(50)
        .run()
        .level_result(CascadeLevel::ModelCheck)
        .cloned()
        .expect("L1 ran")
}

/// The moves of the path L1 gives for the rule `name`, and its last state's
/// place and makers.
fn broken(l1: &LevelResult, name: &str) -> (Vec<String>, String, usize) {
    assert!(!l1.passed, "{}", l1.summary);
    let found = l1
        .verification
        .as_ref()
        .unwrap()
        .counterexamples
        .iter()
        .find(|c| c.property == "CanReach" && c.violated == [name])
        .unwrap_or_else(|| panic!("no path for {name}: {}", l1.summary));
    let moves = found
        .trace
        .iter()
        .filter_map(|(_, action)| action.as_ref().map(|a| a.name.clone()))
        .collect();
    let (last, action) = found.trace.last().unwrap();
    assert!(action.is_none(), "the path ends at the record");
    (
        moves,
        last.status.clone(),
        last.counters.get("makers").copied().unwrap_or_default(),
    )
}

#[test]
fn a_cup_sent_back_that_can_never_be_made_again_breaks_the_rule() {
    let l1 = model_check(&format!("{CUP}{CAN_BE_MADE}"));
    assert!(
        l1.summary
            .starts_with("L1 Model Check FAILED: 7 states explored, 1 counterexample(s)"),
        "{}",
        l1.summary
    );
    let (moves, place, makers) = broken(&l1, "can_still_be_made");
    assert_eq!(moves, ["Add", "Claim", "Finish", "WrongDrink"]);
    assert_eq!((place.as_str(), makers), ("Waiting", 1));
    assert!(
        l1.diagnostics.iter().any(|line| line
            == "rule 'can_still_be_made' broken: the record at the end of this path can never \
                reach Made (4 step(s)):"),
        "{:?}",
        l1.diagnostics
    );
}

/// The whole walk fits in these allowances, but replaying its refusal path
/// would spend the remaining expansion work and lose an already walked step.
#[test]
fn a_completed_walk_keeps_its_refusal_when_replay_would_spend_the_budget() {
    let spec = format!("{CUP}{CAN_BE_MADE}");
    let expected = model_check(&spec);
    let expected_trace = &expected.verification.as_ref().unwrap().counterexamples[0].trace;
    for states in [8, 9] {
        let budget = Budget {
            time: Duration::from_secs(60),
            states,
        };
        let control = VerificationCascade::from_ioa(CUP).with_budget(budget).run();
        let control_l1 = control.level_result(CascadeLevel::ModelCheck).unwrap();
        assert!(control_l1.passed, "{}", control_l1.summary);

        let result = VerificationCascade::from_ioa(&spec)
            .with_fail_fast()
            .with_budget(budget)
            .run();
        assert!(result.stopped.is_none(), "{:?}", result.stopped);
        let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
        assert_eq!(l1.summary, expected.summary);
        let verification = l1.verification.as_ref().unwrap();
        assert!(verification.is_complete);
        let (moves, place, makers) = broken(l1, "can_still_be_made");
        assert_eq!(moves, ["Add", "Claim", "Finish", "WrongDrink"]);
        assert_eq!((place.as_str(), makers), ("Waiting", 1));
        assert_eq!(verification.counterexamples[0].never_reaches, ["Made"]);
        assert_eq!(&verification.counterexamples[0].trace, expected_trace);
    }
}

/// Replaying a completed walk without a budget must not let exploration
/// continue after its budget is spent.
#[test]
fn an_incomplete_walk_still_stops_before_checking_can_reach() {
    let result = VerificationCascade::from_ioa(&format!("{CUP}{CAN_BE_MADE}"))
        .with_budget(Budget {
            time: Duration::from_secs(60),
            states: 7,
        })
        .run();
    let stopped = result.stopped.as_deref().expect("the cascade stopped");
    assert!(
        stopped.starts_with("Cup: L1 Model Check STOPPED:"),
        "{stopped}"
    );
    assert_eq!(
        result.levels.last().unwrap().level,
        CascadeLevel::ModelCheck
    );
    let l1 = result.level_result(CascadeLevel::ModelCheck).unwrap();
    assert!(!l1.passed, "{}", l1.summary);
    let verification = l1.verification.as_ref().unwrap();
    assert!(!verification.is_complete);
    assert!(verification.counterexamples.is_empty());
}

/// When L1 fails, its failures are the whole answer: the note that the same
/// cup can never reach what another on the line can is not shown with them.
#[test]
fn a_failed_check_shows_only_its_failures() {
    let l1 = model_check(&format!("{CUP}{CAN_BE_MADE}"));
    assert_eq!(
        l1.summary,
        "L1 Model Check FAILED: 7 states explored, 1 counterexample(s)"
    );
}

/// Sent back to be made again, the cup keeps its barista and can always
/// still be made.
#[test]
fn a_cup_sent_back_to_be_made_again_keeps_the_rule() {
    let spec = CUP.replace(
        "name = \"WrongDrink\"\nfrom = [\"Made\"]\nto = \"Waiting\"",
        "name = \"WrongDrink\"\nfrom = [\"Made\"]\nto = \"BeingMade\"",
    );
    assert!(spec.contains("to = \"BeingMade\"\n\n[[invariant]]"));
    let l1 = model_check(&format!("{spec}{CAN_BE_MADE}"));
    assert!(l1.passed, "{}", l1.summary);
    assert_eq!(
        l1.summary,
        "L1 Model Check PASSED: 5 states explored, all properties hold"
    );
}

/// With no `from`, the rule holds in every place, and the path is the
/// shortest to a record that breaks it: a refunded cup is never made.
#[test]
fn a_rule_with_no_from_holds_in_every_place() {
    let spec = CUP.replace(
        "name = \"WrongDrink\"\nfrom = [\"Made\"]\nto = \"Waiting\"",
        "name = \"WrongDrink\"\nfrom = [\"Made\"]\nto = \"BeingMade\"",
    );
    let l1 = model_check(&format!(
        "{spec}\n[[liveness]]\nname = \"always_made\"\ncan_reach = [\"Made\"]\n"
    ));
    let (moves, place, _) = broken(&l1, "always_made");
    assert_eq!(moves, ["Add", "RefundCup"]);
    assert_eq!(place, "Refunded");
}

/// A ticket with as many replies as the bound lets it have can't reply
/// again within the walk. The reply past the bound was not walked, so the
/// rule holds as far as the walk went: a bounded pass, not a refusal.
#[test]
fn a_record_whose_future_goes_past_a_bound_gives_a_bounded_pass() {
    let l1 = model_check(&format!(
        "{TICKET}\n[[liveness]]\nname = \"can_always_reply\"\nfrom = [\"InProgress\"]\n\
         can_reach = [\"WaitingOnCustomer\"]\n"
    ));
    assert!(l1.passed, "{}", l1.summary);
    assert!(l1.summary.contains("bounded"), "{}", l1.summary);
}

/// No invariant reads `token`: the only route must be decided from its
/// actual local value, rather than an unknown guard that might hold.
const TOKEN: &str = r#"
[automaton]
name = "Token"
states = ["Waiting", "Done"]
initial = "Waiting"

[[state]]
name = "token"
type = "string"
initial = ""

[[action]]
name = "Finish"
from = ["Waiting"]
to = "Done"
guard = "token == 'ok'"

[[liveness]]
name = "can_finish"
from = ["Waiting"]
can_reach = ["Done"]
"#;

#[test]
fn an_impossible_local_string_guard_cannot_prove_a_route() {
    let l1 = model_check(TOKEN);
    let (moves, place, _) = broken(&l1, "can_finish");
    assert!(
        moves.is_empty(),
        "the initial record already breaks the rule"
    );
    assert_eq!(place, "Waiting");
    let verification = l1.verification.as_ref().unwrap();
    assert!(verification.is_complete);
    assert!(verification.bounded_by.is_empty());
    let counterexample = &verification.counterexamples[0];
    assert_eq!(counterexample.never_reaches, ["Done"]);
    assert_eq!(counterexample.trace[0].0.strings["token"], "".into());
}

#[test]
fn a_true_local_string_guard_keeps_the_route() {
    let spec = TOKEN.replace("initial = \"\"", "initial = \"ok\"");
    let l1 = model_check(&spec);
    assert!(l1.passed, "{}", l1.summary);
    assert!(l1.verification.as_ref().unwrap().is_complete);
}

/// A locally enabled route can be closed by a later input value. The
/// refusal must retain the input and the record it stranded.
#[test]
fn changing_a_guard_only_string_can_close_the_route() {
    let spec = format!(
        "{}{}",
        TOKEN.replace("initial = \"\"", "initial = \"ok\""),
        r#"
[[action]]
name = "ReplaceToken"
from = ["Waiting"]
to = "Waiting"
params = [{ name = "token", type = "string" }]
"#
    );
    // An unrestricted replacement can always restore 'ok'.
    let l1 = model_check(&spec);
    assert!(l1.passed, "{}", l1.summary);
    // A one-shot replacement can strand the record instead.
    let spec = format!(
        "{}\n[[state]]\nname = \"replaced\"\ntype = \"bool\"\ninitial = false\n",
        spec.replace(
            "params = [{ name = \"token\", type = \"string\" }]",
            "params = [{ name = \"token\", type = \"string\" }]\nguard = \"!replaced\"\neffect = [\"replaced = true\"]"
        )
    );
    let l1 = model_check(&spec);
    let (moves, place, _) = broken(&l1, "can_finish");
    assert_eq!(moves, ["ReplaceToken"]);
    assert_eq!(place, "Waiting");
    let counterexample = &l1.verification.as_ref().unwrap().counterexamples[0];
    let action = counterexample.trace[0].1.as_ref().unwrap();
    let last = &counterexample.trace.last().unwrap().0;
    assert_ne!(last.strings["token"], "ok".into());
    assert_eq!(
        action.params["token"],
        temper_verify::ParamValue::Str(last.strings["token"].clone())
    );
}

/// The constraint's source is not read by a guard or invariant, but its
/// value decides whether any input can open the route.
#[test]
fn a_guard_only_string_keeps_its_constraint_dependencies() {
    let spec = format!(
        "{TOKEN}{}",
        r#"
[[state]]
name = "source"
type = "string"
initial = ""

[[action]]
name = "CopyToken"
from = ["Waiting"]
to = "Waiting"
params = [{ name = "token", type = "string" }]

[[action.constraints]]
kind = "param_equals_field"
param = "token"
field = "source"
"#
    );
    let l1 = model_check(&spec);
    let (moves, place, _) = broken(&l1, "can_finish");
    assert!(moves.is_empty());
    assert_eq!(place, "Waiting");
    // Copying a usable source really does open the route from the same
    // initially empty token, including after every copy.
    let enabled = spec.replace(
        "name = \"source\"\ntype = \"string\"\ninitial = \"\"",
        "name = \"source\"\ntype = \"string\"\ninitial = \"ok\"",
    );
    assert_ne!(spec, enabled);
    let l1 = model_check(&enabled);
    assert!(l1.passed, "{}", l1.summary);
    // A source that starts empty can also be supplied with 'ok'.
    let writable = format!(
        "{spec}{}",
        r#"
[[action]]
name = "SupplySource"
from = ["Waiting"]
to = "Waiting"
params = [{ name = "source", type = "string" }]
"#
    );
    let l1 = model_check(&writable);
    assert!(l1.passed, "{}", l1.summary);
}

fn load(rule: &str) -> String {
    let spec = format!("{CUP}\n[[liveness]]\nname = \"rule\"\n{rule}\n");
    parse_automaton_with_liveness(&spec, LivenessEnforcement::WarnOnly)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_default()
}

#[test]
fn a_can_reach_rule_that_also_says_reaches_or_has_actions_fails_to_load() {
    for rule in [
        "can_reach = [\"Made\"]\nreaches = [\"Made\"]",
        "can_reach = [\"Made\"]\nhas_actions = true",
    ] {
        let error = load(rule);
        assert!(
            error.contains("liveness 'rule': `can_reach` takes no `reaches` or `has_actions`"),
            "{rule}: {error}"
        );
    }
    let error = load("can_reach = [\"Made\"]\neventually = \"makers == 0\"");
    assert!(
        error.contains(
            "liveness 'rule': `eventually` takes no `from`, `reaches`, `can_reach` or `has_actions`"
        ),
        "{error}"
    );
}

/// A misspelt `from` would leave the rule checking nothing, and a misspelt
/// place in `can_reach` would refuse every record for a place none can be in.
#[test]
fn a_can_reach_rule_naming_an_undeclared_place_fails_to_load() {
    let error = load("from = [\"Wating\"]\ncan_reach = [\"Made\"]");
    assert!(
        error.contains("liveness 'rule': `from` names no place 'Wating'"),
        "{error}"
    );
    let error = load("from = [\"Waiting\"]\ncan_reach = [\"Mde\"]");
    assert!(
        error.contains("liveness 'rule': `can_reach` names no place 'Mde'"),
        "{error}"
    );
    assert_eq!(load("from = [\"Waiting\"]\ncan_reach = [\"Made\"]"), "");
}

#[path = "can_reach/route_inputs.rs"]
mod route_inputs;

#[path = "can_reach/review_constraints.rs"]
mod review_constraints;

#[path = "can_reach/entry_rules.rs"]
mod entry_rules;
