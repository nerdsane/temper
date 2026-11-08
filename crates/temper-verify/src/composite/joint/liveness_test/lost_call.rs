//! A module call lost to a restart: answered by its `on_failure` when it
//! names one, as a failed call is; otherwise the record is left stuck. An
//! `on_failure` answers no call that ends with no answer.

use super::*;

#[test]
fn a_lost_call_with_no_on_failure_after_a_one_shot_timer_leaves_the_record_stuck() {
    // Nothing answers a call that names no `on_failure` when it fails or is
    // lost to a restart, and the timer has already fired its one `Again`.
    // The two end the same way, so the shortest path may show either.
    let result = check_specs(COUNTING_APP, &counting_place(true), SAME_RECORD);
    let LivenessOutcome::Stuck { cases, .. } = &result.outcome else {
        panic!("a lost call after the timer must stay stuck: {result}");
    };
    assert!(
        cases.iter().any(|case| {
            let actions: Vec<&str> = case
                .path
                .iter()
                .filter_map(|step| step.action.as_deref())
                .collect();
            actions
                .iter()
                .position(|a| a.contains("(timer)"))
                .is_some_and(|timer| {
                    actions[timer + 1..].iter().any(|a| {
                        a.ends_with(" fails") || a.ends_with("is lost: the server restarts")
                    })
                })
        }),
        "{result}"
    );
}

#[test]
fn a_lost_call_with_an_on_failure_is_answered_by_it() {
    // Every call names `on_failure = "Again"`. A call lost to a restart is
    // answered by it as a failed call is, so `Again` asks the module again
    // and neither leaves the record at rest. Only a call that ends with no
    // answer does, once the one-shot timer has fired.
    let place = counting_place(true)
        .replace(
            "on_success = \"Placed\"",
            "on_success = \"Placed\"\non_failure = \"Again\"",
        )
        .replace(
            "module = \"steps\"\n\n[[action]]\nname = \"Again\"",
            "module = \"steps\"\non_success = \"Placed\"\non_failure = \"Again\"\n\n[[action]]\nname = \"Again\"",
        );
    assert_eq!(
        place.matches("on_failure = \"Again\"").count(),
        2,
        "{place}"
    );
    let result = check_specs(COUNTING_APP, &place, SAME_RECORD);
    let LivenessOutcome::Stuck { cases, .. } = &result.outcome else {
        panic!("a call can still end with no answer: {result}");
    };
    for case in cases {
        let last = case
            .path
            .iter()
            .rev()
            .find_map(|step| step.action.as_deref());
        assert!(
            last.is_some_and(|action| action.ends_with("ends with no answer")),
            "{result}"
        );
    }
}
