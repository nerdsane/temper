use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::super::room::{Bytes, Progress};

use super::*;

/// An app installed once at a commit, which its place is told to bring as
/// the service `place`.
const APP: &str = r#"
[automaton]
name = "App"
states = ["Unmade", "Live"]
initial = "Unmade"

[[state]]
name = "commit"
type = "string"
initial = ""

[[action]]
name = "Install"
kind = "input"
from = ["Unmade"]
to = "Live"
params = [{ name = "commit", type = "string" }]

[[action.constraints]]
kind = "param_nonempty"
param = "commit"

[[action.triggers]]
name = "place"
kind = "entity"
target_entity = "Place"
target_action = "Bring"
principal = "place"

[action.triggers.args]
commit = "commit"

[action.triggers.resolve_target]
kind = "same_id"
"#;

/// The place: a module brings the commit, may make it wait or give up, and
/// gives up by itself after a while. `nudge_from` is where outside code may
/// ask again.
fn place(nudge_from: &str) -> String {
    format!(
        r#"
[automaton]
name = "Place"
states = ["Unplaced", "Bringing", "Waiting", "Placed", "Stuck"]
initial = "Unplaced"

[[state]]
name = "commit"
type = "string"
initial = ""

[[action]]
name = "Bring"
kind = "input"
from = ["Unplaced"]
to = "Bringing"
params = [{{ name = "commit", type = "string" }}]

[[action.triggers]]
name = "place"
kind = "wasm"
module = "steps"
on_failure = "Waiting"

[[action]]
name = "Nudge"
kind = "input"
from = {nudge_from}
to = "Bringing"

[[action.triggers]]
name = "place"
kind = "wasm"
module = "steps"
on_failure = "Waiting"

[[action]]
name = "Placed"
kind = "internal"
from = ["Bringing"]
to = "Placed"

[[action]]
name = "Waiting"
kind = "internal"
from = ["Bringing"]
to = "Waiting"

[[action]]
name = "GaveUp"
kind = "internal"
from = ["Bringing"]
to = "Stuck"

[[state_timeout]]
state = "Bringing"
after_seconds = 90
on_timeout = "GaveUp"

[[liveness]]
name = "placed"
eventually = "App[Id].status == 'Live' => (status == 'Placed' && App[Id].commit == commit)"
relies_on = ["Nudge"]
"#
    )
}

/// Anyone outside may install and nudge; `bring` is the condition under
/// which Cedar lets a Bring through.
fn policies(bring: &str) -> String {
    format!(
        r#"
permit(principal, action in [Action::"Install", Action::"Nudge"], resource);
permit(principal, action == Action::"Bring", resource is Place) when {{ {bring} }};
"#
    )
}

/// The place's service, bringing for the app with its own id.
const SAME_RECORD: &str = r#"principal == Agent::"service:place" &&
    context has triggerSourceEntityId && resource has id &&
    context.triggerSourceEntityId == resource.id"#;

fn check(nudge_from: &str, bring: &str) -> JointLivenessResult {
    check_specs(APP, &place(nudge_from), bring)
}

fn check_specs(app: &str, place: &str, bring: &str) -> JointLivenessResult {
    check_specs_with_policies(app, place, &policies(bring))
}

fn check_specs_with_policies(app: &str, place: &str, policies: &str) -> JointLivenessResult {
    let automatons: Vec<Automaton> = [app, place]
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(policies).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let mut results = verify_joint_liveness(&refs, Some(&analysis), bound);
    assert_eq!(results.len(), 1);
    results.remove(0)
}

/// The record statuses each stuck case comes to rest in, in words.
fn resting(result: &JointLivenessResult) -> Vec<String> {
    let LivenessOutcome::Stuck { cases, .. } = &result.outcome else {
        panic!("expected a stuck outcome, got {result}");
    };
    cases
        .iter()
        .map(|case| case.path.last().expect("a path has a start").state.clone())
        .collect()
}

#[test]
fn a_place_that_gave_up_with_no_way_back_never_settles() {
    let result = check(r#"["Waiting"]"#, SAME_RECORD);
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("Place=Stuck")),
        "{result}"
    );
}

#[test]
fn a_nudge_relied_on_from_where_it_gives_up_settles() {
    let result = check(r#"["Waiting", "Stuck"]"#, SAME_RECORD);
    assert!(result.holds(), "{result}");
}

#[test]
fn a_relied_on_request_whose_sender_fills_its_parameters_settles() {
    let nudge = r#"from = ["Waiting", "Stuck"]
to = "Bringing"
"#;
    let place = place(r#"["Waiting", "Stuck"]"#).replacen(
        nudge,
        &format!(
            "{nudge}params = [{{ name = \"why\", type = \"string\" }}]\n\n\
             [[action.constraints]]\nkind = \"param_nonempty\"\nparam = \"why\"\n"
        ),
        1,
    );
    let result = check_specs(APP, &place, SAME_RECORD);
    assert!(result.holds(), "{result}");
}

#[test]
fn a_reaction_cedar_may_refuse_is_not_counted_on() {
    let bring = r#"principal == Agent::"service:place" && resource.owner == "someone""#;
    let result = check(r#"["Waiting", "Stuck"]"#, bring);
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("Place=Unplaced")),
        "{result}"
    );
}

#[test]
fn a_reaction_cedar_refuses_its_service_leaves_the_place_behind() {
    let bring = r#"principal == Agent::"service:someone-else""#;
    let result = check(r#"["Waiting", "Stuck"]"#, bring);
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("Place=Unplaced")),
        "{result}"
    );
}

/// An app installed again and again, which counts each install and sends
/// the count with the commit for its place to bring.
const COUNTING_APP: &str = r#"
[automaton]
name = "App"
states = ["Unmade", "Live"]
initial = "Unmade"

[[state]]
name = "commit"
type = "string"
initial = ""

[[state]]
name = "brought"
type = "counter"
initial = 0

[[action]]
name = "Install"
kind = "input"
from = ["Unmade", "Live"]
to = "Live"
params = [{ name = "commit", type = "string" }]
effect = ["brought += 1"]

[[action.constraints]]
kind = "param_nonempty"
param = "commit"

[[action.triggers]]
name = "place"
kind = "entity"
target_entity = "Place"
target_action = "Bring"
principal = "place"

[action.triggers.args]
commit = "commit"
brought = "brought"

[action.triggers.resolve_target]
kind = "same_id"
"#;

/// A place that brings whatever it is sent, or with `newer` only a count
/// greater than the last it took. Its module answers for the commit it was
/// asked to bring, and a timer asks again.
fn counting_place(newer: bool) -> String {
    let newer = if newer {
        "[[action.constraints]]\nkind = \"param_greater_than_field\"\nparam = \"brought\"\nfield = \"brought\"\n"
    } else {
        ""
    };
    format!(
        r#"
[automaton]
name = "Place"
states = ["Unplaced", "Bringing", "Placed"]
initial = "Unplaced"

[[state]]
name = "commit"
type = "string"
initial = ""

[[state]]
name = "brought"
type = "counter"
initial = 0

[[action]]
name = "Bring"
kind = "input"
from = ["Unplaced", "Bringing", "Placed"]
to = "Bringing"
params = [{{ name = "commit", type = "string" }}, {{ name = "brought", type = "counter" }}]
effect = ["brought = params.brought"]

{newer}
[[action.triggers]]
name = "place"
kind = "wasm"
module = "steps"

[[action]]
name = "Again"
kind = "internal"
from = ["Bringing"]
to = "Bringing"

[[action.triggers]]
name = "place"
kind = "wasm"
module = "steps"
on_success = "Placed"

[[action]]
name = "Placed"
kind = "internal"
from = ["Bringing"]
to = "Placed"
params = [{{ name = "placed_commit", type = "string" }}]

[[action.constraints]]
kind = "param_equals_field"
param = "placed_commit"
field = "commit"

[[state_timeout]]
state = "Bringing"
after_seconds = 60
on_timeout = "Again"

[[liveness]]
name = "placed"
eventually = "App[Id].status == 'Live' => (status == 'Placed' && App[Id].commit == commit)"
"#
    )
}

#[test]
fn a_place_that_takes_only_newer_counts_settles() {
    let place = counting_place(true).replace(
        "on_timeout = \"Again\"",
        "on_timeout = \"Again\"\nreset_on = [\"Again\"]",
    );
    let result = check_specs(COUNTING_APP, &place, SAME_RECORD);
    assert!(result.holds(), "{result}");
}

#[test]
fn a_late_bring_for_an_older_commit_leaves_the_place_behind() {
    let result = check_specs(COUNTING_APP, &counting_place(false), SAME_RECORD);
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("Place=Placed")),
        "{result}"
    );
}

#[test]
fn a_lost_call_after_a_one_shot_timer_leaves_the_record_stuck() {
    // A reported failure retries the module; a lost call has no callback.
    let place = counting_place(true).replace(
        "on_success = \"Placed\"",
        "on_success = \"Placed\"\non_failure = \"Again\"",
    );
    let result = check_specs(COUNTING_APP, &place, SAME_RECORD);
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
                .is_some_and(|timer| actions[timer + 1..].iter().any(|a| a.contains("is lost")))
        }),
        "{result}"
    );
}

#[test]
fn a_second_comparison_on_the_same_sequence_parameter_is_not_certain_progress() {
    let app = COUNTING_APP.replace("from = [\"Unmade\", \"Live\"]", "from = [\"Unmade\"]");
    let place = counting_place(true)
        .replace(
            "[[action]]\nname = \"Bring\"",
            "[[state]]\nname = \"floor\"\ntype = \"counter\"\ninitial = 2\n\n[[action]]\nname = \"Bring\"",
        )
        .replace(
            "field = \"brought\"",
            "field = \"brought\"\n\n[[action.constraints]]\nkind = \"param_greater_than_field\"\nparam = \"brought\"\nfield = \"floor\"",
        )
        .replace(
            "on_timeout = \"Again\"",
            "on_timeout = \"Again\"\nreset_on = [\"Again\"]",
        );
    // Install sends 1: it exceeds brought=0, but not floor=2. The
    // unmodelled second comparison must leave refusal as a possible outcome.
    let result = check_specs(&app, &place, SAME_RECORD);
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("Place=Unplaced")),
        "{result}"
    );
}

#[test]
fn a_reaction_forbidden_by_equal_cedar_sets_leaves_the_place_behind() {
    let policies = r#"
        permit(principal, action, resource);
        forbid(principal, action == Action::"Bring", resource is Place)
        when { [1, 2] == [2, 1, 1] };
    "#;
    let result = check_specs_with_policies(APP, &place(r#"["Waiting", "Stuck"]"#), policies);
    assert!(
        resting(&result)
            .iter()
            .any(|state| state.contains("Place=Unplaced")),
        "{result}"
    );
}

#[test]
fn oversized_parameter_expansion_is_incomplete() {
    // Seven retained counters give 4^7 = 16,384 assignments (budget: 4,096).
    let fields = (0..7)
        .map(|i| format!("[[state]]\nname = 'c{i}'\ntype = 'counter'\ninitial = 0\n"))
        .collect::<String>();
    let params = (0..7)
        .map(|i| format!("{{ name = 'p{i}', type = 'counter' }}"))
        .collect::<Vec<_>>()
        .join(", ");
    let effects = (0..7)
        .map(|i| format!("'c{i} = params.p{i}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let goal = (0..7)
        .map(|i| format!("c{i} == 0"))
        .collect::<Vec<_>>()
        .join(" && ");
    let source = format!(
        "[automaton]\nname = 'Counter'\nstates = ['Ready']\ninitial = 'Ready'\n\n\
         {fields}\n[[action]]\nname = 'Set'\nkind = 'input'\nfrom = ['Ready']\n\
         to = 'Ready'\nparams = [{params}]\neffect = [{effects}]\n\n\
         [[liveness]]\nname = 'zero'\neventually = \"{goal}\"\n"
    );
    let automaton = parse_automaton(&source).expect("fixture parses");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let results = verify_joint_liveness(&[&automaton], None, bound);
    assert_eq!(results.len(), 1);
    let result = &results[0];
    let LivenessOutcome::Incomplete { reason } = &result.outcome else {
        panic!("expected parameter expansion to be incomplete: {result}");
    };
    assert!(
        reason.contains("Counter.Set parameter expansion"),
        "{result}"
    );
    assert_eq!(result.states, 0);
    assert_eq!(result.in_flight_bound, None);

    // The same action fits once slicing removes fields the goal does not read.
    let sliced = source.replace(&goal, "status == 'Ready'");
    let automaton = parse_automaton(&sliced).expect("fixture parses");
    let results = verify_joint_liveness(&[&automaton], None, bound);
    assert_eq!(results.len(), 1);
    assert!(results[0].holds(), "{}", results[0]);
}

mod cause;
