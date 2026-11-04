//! The fairness check, on small apps whose answer tla-ts gave for the same
//! system written in TLA+ (`tests/fair_models`, `scripts/fair-cross-check.mjs`).

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::super::room::{Bytes, Progress};
use super::super::{
    Bound, FairOutcome, JointLivenessResult, LivenessOutcome, verify_joint_liveness,
};

macro_rules! model {
    ($name:literal) => {
        include_str!(concat!("../../../tests/fair_models/", $name, ".ioa.toml"))
    };
}

fn check(source: &str) -> JointLivenessResult {
    check_on(source, Bytes::new(1 << 30))
}

fn check_on(source: &str, room: Bytes) -> JointLivenessResult {
    let automatons: Vec<Automaton> = source
        .split("\n---\n")
        .map(|part| parse_automaton(part).expect("model parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let progress = Progress::default();
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_liveness(&refs, None, bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

fn fair(result: &JointLivenessResult) -> &FairOutcome {
    &result
        .fair
        .as_ref()
        .expect("the rule asks for fairness")
        .outcome
}

fn assert_holds(result: &JointLivenessResult) {
    assert!(
        matches!(
            fair(result),
            FairOutcome::Holds {
                kept_away: None,
                may_be_kept_away: None,
            }
        ),
        "{result}"
    );
}

/// The statuses a broken verdict's cycle passes through, in order, ending where
/// it began, and the status it began at.
fn broken_cycle(result: &JointLivenessResult) -> (String, Vec<String>) {
    let FairOutcome::Broken { lasso } = fair(result) else {
        panic!("expected BROKEN, got {result}");
    };
    let status = |state: &str| {
        state
            .split(['=', '<', ' '])
            .nth(1)
            .unwrap_or(state)
            .to_string()
    };
    (
        status(&lasso.prefix.last().expect("a lasso has a start").state),
        lasso.cycle.iter().map(|step| status(&step.state)).collect(),
    )
}

fn can_reach_holds(result: &JointLivenessResult) -> bool {
    matches!(result.outcome, LivenessOutcome::Holds)
}

#[test]
fn a_loop_that_dodges_a_weakly_fair_exit_never_reaches_the_goal() {
    let result = check(model!("loop_weak"));
    assert!(can_reach_holds(&result), "{result}");
    let (at, cycle) = broken_cycle(&result);
    assert_eq!(at, "A", "{result}");
    assert_eq!(cycle, ["B", "A"], "{result}");
}

#[test]
fn a_strongly_fair_exit_breaks_the_loop() {
    let result = check(model!("loop_strong"));
    assert!(can_reach_holds(&result), "{result}");
    assert_holds(&result);
}

#[test]
fn a_fair_cycle_with_no_way_to_the_goal_is_broken_under_either_fairness() {
    for result in [check(model!("never_weak")), check(model!("never_strong"))] {
        assert!(
            matches!(result.outcome, LivenessOutcome::Stuck { .. }),
            "{result}"
        );
        assert!(
            matches!(fair(&result), FairOutcome::Broken { .. }),
            "{result}"
        );
    }
}

#[test]
fn a_weakly_fair_move_possible_all_round_the_loop_is_taken() {
    assert_holds(&check(model!("always_possible")));
}

#[test]
fn a_request_nobody_owes_leaves_the_record_where_it_is() {
    let result = check(model!("nobody_comes"));
    let (at, cycle) = broken_cycle(&result);
    assert_eq!(at, "A", "{result}");
    assert!(cycle.is_empty(), "{result}");
}

#[test]
fn the_kernel_owes_a_call_its_end_and_not_its_success() {
    let weak = check(model!("retry_weak"));
    assert!(can_reach_holds(&weak), "{weak}");
    assert!(matches!(fair(&weak), FairOutcome::Broken { .. }), "{weak}");
    assert_holds(&check(model!("retry_strong")));
}

#[test]
fn a_call_that_ends_with_no_answer_needs_something_else_to_move_its_record() {
    let result = check(model!("silent_call"));
    assert!(
        matches!(fair(&result), FairOutcome::Broken { .. }),
        "{result}"
    );
    assert_holds(&check(model!("silent_call_timer")));
}

#[test]
fn strong_fairness_leaves_the_inner_cycle_that_avoids_its_move() {
    let result = check(model!("rings_one_strong"));
    let (_, cycle) = broken_cycle(&result);
    assert!(
        !cycle.iter().any(|status| status == "A"),
        "the inner cycle never goes through A: {result}"
    );
    assert_holds(&check(model!("rings_two_strong")));
}

#[test]
fn a_reaction_in_flight_is_delivered() {
    assert_holds(&check(&format!(
        "{}\n---\n{}",
        model!("reaction_delivered"),
        model!("reaction_delivered.Source")
    )));
}

#[test]
fn someone_who_keeps_acting_is_reported_beside_a_rule_that_holds_when_left_alone() {
    let result = check(model!("kept_away"));
    let FairOutcome::Holds {
        kept_away: Some(lasso),
        ..
    } = fair(&result)
    else {
        panic!("expected a lasso beside HOLDS, got {result}");
    };
    assert!(
        lasso
            .cycle
            .iter()
            .any(|step| step.action.as_deref().is_some_and(|a| a.contains("Flip"))),
        "{result}"
    );
}

/// Cedar may let someone keep it from the condition, or may not: that depends
/// on the request, so HOLDS says so without claiming the cycle is there.
#[test]
fn someone_who_may_keep_acting_is_reported_as_not_sure_beside_a_rule_that_holds() {
    let automaton = parse_automaton(model!("kept_away")).expect("model parses");
    let analysis = SendAnalysis::new(
        r#"
permit(principal, action in [Action::"Begin", Action::"Exit"], resource);
permit(principal, action == Action::"Flip", resource) when { context.urgent == true };
"#,
    )
    .expect("its Cedar parses");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_liveness(&[&automaton], Some(&analysis), bound);
    assert_eq!(results.len(), 1);
    let result = results.remove(0);
    let FairOutcome::Holds {
        kept_away: None,
        may_be_kept_away: Some(why),
    } = fair(&result)
    else {
        panic!("expected a note beside HOLDS, got {result}");
    };
    assert!(why.contains("Flip"), "{result}");
    assert!(result.to_string().contains("HOLDS"), "{result}");
}

#[test]
fn a_rule_that_does_not_ask_stays_a_can_reach_rule() {
    let source = model!("loop_weak").replace("fairness = { weak = [\"Exit\"] }\n", "");
    let result = check(&source);
    assert!(can_reach_holds(&result), "{result}");
    assert!(result.fair.is_none(), "{result}");
}

#[test]
fn a_rule_cannot_name_an_action_its_entity_lacks_or_one_twice() {
    for (fairness, says) in [
        ("{ weak = [\"Nope\"] }", "names no action"),
        (
            "{ weak = [\"Exit\"], strong = [\"Exit\"] }",
            "both weak and strong",
        ),
    ] {
        let source = model!("loop_weak").replace("{ weak = [\"Exit\"] }", fairness);
        let error = parse_automaton(&source).expect_err("refused").to_string();
        assert!(error.contains(says), "{error}");
    }
}

/// The status a broken verdict's cycle ends at, and the moves on it.
fn broken_moves(result: &JointLivenessResult) -> Vec<String> {
    let FairOutcome::Broken { lasso } = fair(result) else {
        panic!("expected BROKEN, got {result}");
    };
    lasso
        .cycle
        .iter()
        .map(|step| step.action.clone().unwrap_or_default())
        .collect()
}

/// A call answered by a move that stays where it is ended all the same: the
/// joint state has no call number to show it. Polling forever is fair.
#[test]
fn a_call_answered_by_staying_can_go_on_forever() {
    let result = check(model!("poll_self"));
    let moves = broken_moves(&result);
    assert!(moves.iter().any(|mv| mv.contains("Still")), "{result}");
}

/// A `reset_on` move starts the clock again, so the timer is never due while
/// it keeps coming, whether or not it changes anything the state holds.
#[test]
fn a_move_that_resets_a_timer_can_keep_it_from_firing() {
    for source in [model!("heartbeat"), model!("heartbeat_field")] {
        let result = check(source);
        let moves = broken_moves(&result);
        assert!(moves.iter().any(|mv| mv.contains("Beat")), "{result}");
    }
}

/// Leaving a state cancels its timer, so each state's timer is owed on its own
/// even when two of them fire one action.
#[test]
fn timers_of_two_states_are_owed_apart_when_they_share_an_action() {
    for source in [model!("shared_timer"), model!("split_timer")] {
        let result = check(source);
        assert!(
            matches!(fair(&result), FairOutcome::Broken { .. }),
            "{result}"
        );
    }
}

/// Two calls to one module are two endings owed: the second's answers do not
/// let the first wait forever.
#[test]
fn two_calls_to_one_module_are_owed_apart() {
    assert_holds(&check(model!("merged_calls")));
}

/// A walk on many threads numbers states in another order and leaves its moves
/// interleaved, and the verdict does not move.
#[test]
fn a_walk_on_many_threads_gives_the_same_verdicts() {
    for source in [
        model!("loop_weak"),
        model!("loop_strong"),
        model!("never_strong"),
        model!("retry_weak"),
        model!("retry_strong"),
        model!("rings_one_strong"),
        model!("rings_two_strong"),
    ] {
        let one = check(source);
        for _ in 0..8 {
            let many = check_on(source, Bytes::new(1 << 30).with_threads(4));
            assert_eq!(one.states, many.states, "{many}");
            assert_eq!(
                std::mem::discriminant(fair(&one)),
                std::mem::discriminant(fair(&many)),
                "{one}\n{many}"
            );
        }
    }
}

/// Nothing leaves the start, so no state the walk finds can miss the condition.
#[test]
fn a_rule_the_walk_never_sees_missed_checks_nothing() {
    let source = model!("loop_weak")
        .replace("from = [\"Idle\"]", "from = [\"Out\"]")
        .replace(
            "eventually = \"status == 'Idle' || status == 'Out'\"",
            "eventually = \"status != 'A'\"",
        );
    let result = check(&source);
    assert!(can_reach_holds(&result), "{result}");
    let FairOutcome::Vacuous { why } = fair(&result) else {
        panic!("expected CHECKS NOTHING, got {result}");
    };
    assert!(why.contains("status != 'A'"), "{result}");
}

fn not_checked_naming(result: &JointLivenessResult, names: &str) {
    let FairOutcome::Incomplete { reason } = fair(result) else {
        panic!("expected NOT CHECKED, got {result}");
    };
    assert!(reason.contains(names), "{result}");
}

/// A timer whose guard reads a record outside the rule's walk: the model cannot
/// say the cycle runs, nor that it does not, so the rule is neither.
#[test]
fn a_cycle_through_a_guard_the_model_cannot_decide_is_not_checked() {
    let source = model!("loop_weak").replace(
        "name = \"Go\"\nkind = \"internal\"\nfrom = [\"A\"]\nto = \"B\"",
        "name = \"Go\"\nkind = \"internal\"\nfrom = [\"A\"]\nto = \"B\"\nguard = \"Other[Id].status == 'On'\"",
    );
    let source = format!("{source}\n---\n{OTHER}");
    not_checked_naming(&check(&source), "Loop.Go (a timer)");
}

const OTHER: &str = r#"
[automaton]
name = "Other"
states = ["Off", "On"]
initial = "Off"

[[action]]
name = "Turn"
kind = "input"
from = ["Off"]
to = "On"
"#;

/// A note only a reaction from a type outside the rule's scope finishes.
const NOTE_FINISHED_FROM_OUTSIDE: &str = r#"
[automaton]
name = "Note"
states = ["Idle", "Waiting", "Done"]
initial = "Idle"

[[action]]
name = "Begin"
kind = "input"
from = ["Idle"]
to = "Waiting"

[[action]]
name = "Finish"
kind = "internal"
from = ["Waiting"]
to = "Done"

[[liveness]]
name = "Ends"
eventually = "status != 'Waiting'"
fairness = {}
---
[automaton]
name = "Push"
states = ["Idle", "Sent"]
initial = "Idle"

[[action]]
name = "Send"
kind = "input"
from = ["Idle"]
to = "Sent"

[[action.triggers]]
name = "finish"
kind = "entity"
target_entity = "Note"
target_action = "Finish"

[action.triggers.resolve_target]
kind = "same_id"
"#;

/// Nothing the model holds says such a reaction comes, so HOLDS may not count
/// on it, and nothing says it stays away, so BROKEN may not either.
#[test]
fn a_reaction_from_outside_the_scope_makes_neither_verdict() {
    not_checked_naming(
        &check(NOTE_FINISHED_FROM_OUTSIDE),
        "Note.Finish (a reaction from outside the rule's scope)",
    );
}

/// Fairness says a move is taken, not where it leads: taking this one forever
/// only goes round again.
#[test]
fn fairness_on_a_move_that_leads_back_into_the_cycle_changes_nothing() {
    let source = model!("never_weak").replace(
        "{ weak = [\"Exit\"] }",
        "{ weak = [\"Exit\"], strong = [\"Return\"] }",
    );
    assert!(
        matches!(fair(&check(&source)), FairOutcome::Broken { .. }),
        "Return still leads back into the loop"
    );
}
