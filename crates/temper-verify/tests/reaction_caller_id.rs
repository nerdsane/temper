//! A move the kernel makes itself (a reaction, a timer, a module's answer)
//! that reaches an action whose parameter the kernel binds (`source`) has the
//! kernel bind it from whoever set the chain off. No one may send it, and a
//! reaction that runs as its own service principal names no caller, so the
//! kernel refuses the move.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};
use temper_verify::composite::{
    Bound, Bytes, JointOutcome, LivenessOutcome, Progress, verify_joint_invariants,
    verify_joint_liveness,
};

/// A parent whose Ping makes its child Stamp.
const PARENT: &str = r#"
[automaton]
name = "Parent"
states = ["Open", "Pinged"]
initial = "Open"

[[action]]
name = "Ping"
kind = "input"
from = ["Open"]
to = "Pinged"

[[action.triggers]]
name = "stamp"
kind = "entity"
target_entity = "Child"
target_action = "Stamp"

[action.triggers.resolve_target]
kind = "same_id"
"#;

/// A child only a reaction stamps, with whoever set it off.
const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Stamped"]
initial = "Waiting"
strict_action_params = true

[[state]]
name = "stamped_by"
type = "string"
initial = ""

[[action]]
name = "Stamp"
kind = "internal"
from = ["Waiting"]
to = "Stamped"
params = [{ name = "stamped_by", type = "string", source = "authenticated_subject" }]

[[invariant]]
name = "stamped_by_no_one"
assert = "status != 'Stamped' || stamped_by == '' || Parent[Id].status == 'Open'"
"#;

/// A record whose Ping makes it Stamp, as the reaction's own principal.
const AS_SERVICE: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Pinged", "Stamped"]
initial = "Waiting"
strict_action_params = true

[[state]]
name = "stamped_by"
type = "string"
initial = ""

[[action]]
name = "Ping"
kind = "input"
from = ["Waiting"]
to = "Pinged"

[[action.triggers]]
name = "stamp"
kind = "entity"
target_entity = "Child"
target_action = "Stamp"
principal = "stamper"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Stamp"
kind = "internal"
from = ["Pinged"]
to = "Stamped"
params = [{ name = "stamped_by", type = "string", source = "authenticated_subject" }]

[[liveness]]
name = "gets_stamped"
eventually = "status != 'Pinged'"
"#;

const CEDAR: &str = r#"
permit(principal, action == Action::"Ping", resource);
permit(principal, action == Action::"Stamp", resource) when { context has via && context.via != "request" };
"#;

fn parsed(sources: &[&str]) -> Vec<Automaton> {
    sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect()
}

/// Whether every joint invariant of `parent` and `child` holds.
fn holds(parent: &str, child: &str) -> bool {
    holds_under(&[parent, child], CEDAR)
}

/// Whether every joint invariant of `sources` holds under `policies`.
fn holds_under(sources: &[&str], policies: &str) -> bool {
    outcomes_under(sources, policies)
        .iter()
        .all(|outcome| matches!(outcome, JointOutcome::Holds))
}

/// The outcome of each joint invariant of `sources` under `policies`.
fn outcomes_under(sources: &[&str], policies: &str) -> Vec<JointOutcome> {
    let automatons = parsed(sources);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(policies).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let results = verify_joint_invariants(&refs, Some(&analysis), bound);
    assert!(!results.is_empty());
    results.into_iter().map(|result| result.outcome).collect()
}

#[test]
fn a_reaction_writes_the_callers_id_or_label_into_a_bound_field() {
    let nonempty = CHILD.replace("stamped_by == ''", "stamped_by != ''");
    assert!(!holds(PARENT, CHILD));
    assert!(holds(PARENT, &nonempty));
    let label = CHILD.replace("authenticated_subject", "authenticated_agent_label");
    assert!(!holds(PARENT, &label));
}

#[test]
fn a_reaction_that_sends_a_bound_field_or_runs_as_a_service_is_refused() {
    let named = PARENT.replace(
        "target_action = \"Stamp\"",
        "target_action = \"Stamp\"\nprincipal = \"stamper\"",
    );
    let nonempty = CHILD.replace("stamped_by == ''", "stamped_by != ''");
    assert!(holds(&named, &nonempty));
    let sent = PARENT.replace(
        "target_action = \"Stamp\"",
        "target_action = \"Stamp\"\nargs = { stamped_by = \"'x'\" }",
    );
    assert!(holds(&sent, CHILD));
}

/// The liveness outcomes of `source` under `policies`.
fn liveness(source: &str, policies: &str) -> Vec<LivenessOutcome> {
    let automatons = parsed(&[source]);
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(policies).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    let results = verify_joint_liveness(&refs, Some(&analysis), bound);
    assert!(!results.is_empty());
    results.into_iter().map(|result| result.outcome).collect()
}

/// Nothing reads the bound field here, and the kernel still refuses.
#[test]
fn a_move_the_kernel_always_refuses_never_settles() {
    let sent = AS_SERVICE.replace("principal = \"stamper\"", "args = { stamped_by = \"'x'\" }");
    for source in [AS_SERVICE, &sent] {
        let outcomes = liveness(source, "permit(principal, action, resource);");
        assert!(
            outcomes
                .iter()
                .any(|outcome| matches!(outcome, LivenessOutcome::Stuck { .. })),
            "{source}: {outcomes:#?}"
        );
    }
}

/// A sender that surely names a caller (an agent: only a public endpoint's
/// anonymous caller names none) has the kernel surely bind, so the move is
/// surely taken and the rule surely holds.
#[test]
fn a_reaction_for_a_sender_who_names_a_caller_surely_binds() {
    let inherits = AS_SERVICE.replace("principal = \"stamper\"\n", "");
    assert_ne!(inherits, AS_SERVICE);
    let outcomes = liveness(&inherits, "permit(principal is Agent, action, resource);");
    assert!(
        outcomes
            .iter()
            .all(|outcome| matches!(outcome, LivenessOutcome::Holds)),
        "{outcomes:#?}"
    );
}

/// A request that binds a field: the sender chose every other parameter,
/// and the kernel writes the caller's id, never the empty string.
#[test]
fn a_request_binds_the_callers_id_too() {
    let parent = r#"
[automaton]
name = "Parent"
states = ["Open"]
initial = "Open"

[[action]]
name = "Touch"
kind = "input"
from = ["Open"]
to = "Open"
"#;
    let child = r#"
[automaton]
name = "Child"
states = ["Waiting", "Approved"]
initial = "Waiting"
strict_action_params = true

[[state]]
name = "approved_by"
type = "string"
initial = ""

[[action]]
name = "Approve"
kind = "input"
from = ["Waiting"]
to = "Approved"
guard = "Parent[Id].status == 'Open'"
params = [{ name = "approved_by", type = "string", source = "authenticated_subject" }]

[[invariant]]
name = "approved_by_no_one"
assert = "status != 'Approved' || approved_by == '' || Parent[Id].status != 'Open'"
"#;
    assert!(!holds_under(
        &[parent, child],
        "permit(principal is Agent, action, resource);"
    ));
}

/// What one move of a chain binds, the next one binds the same: the kernel
/// takes both from the caller who set the chain off.
#[test]
fn a_chain_binds_one_caller_throughout() {
    let parent = r#"
[automaton]
name = "Parent"
states = ["Waiting", "Opened"]
initial = "Waiting"
strict_action_params = true

[[state]]
name = "opener"
type = "string"
initial = ""

[[action]]
name = "Open"
kind = "input"
from = ["Waiting"]
to = "Opened"
params = [{ name = "opener", type = "string", source = "authenticated_subject" }]

[[action.triggers]]
name = "stamp"
kind = "entity"
target_entity = "Child"
target_action = "Stamp"

[action.triggers.resolve_target]
kind = "same_id"
"#;
    let child = CHILD.replace(
        "stamped_by == '' || Parent[Id].status == 'Open'",
        "stamped_by == '' || stamped_by == Parent[Id].opener",
    );
    let policies = r#"
permit(principal is Agent, action, resource) unless { action == Action::"Stamp" };
permit(principal is Agent, action == Action::"Stamp", resource) when { context has via && context.via != "request" };
"#;
    assert!(holds_under(&[parent, &child], policies));
}

/// A constraint on a bound field may refuse the caller the runtime has, so
/// the move is not sure to be taken. The rule on `owner` makes the walk
/// track it, so only the constraint leaves the move unsure.
#[test]
fn a_constraint_on_a_bound_field_may_refuse_the_move() {
    let constrained = AS_SERVICE.replace("principal = \"stamper\"\n", "").replace(
        "[[liveness]]",
        r#"[[state]]
name = "owner"
type = "string"
initial = "alice"

[[action.constraints]]
kind = "param_equals_field"
param = "stamped_by"
field = "owner"

[[invariant]]
name = "owner_kept"
assert = "owner == 'alice'"

[[liveness]]"#,
    );
    let outcomes = liveness(
        &constrained,
        "permit(principal is Agent, action, resource);",
    );
    assert!(
        !outcomes
            .iter()
            .any(|outcome| matches!(outcome, LivenessOutcome::Holds)),
        "{outcomes:#?}"
    );
}

/// A sender the policies tell apart as anonymous names no caller, so a
/// reaction it sets off that binds is refused and never settles.
#[test]
fn an_anonymous_sender_told_apart_by_another_rule_is_refused() {
    let inherits = AS_SERVICE.replace("principal = \"stamper\"\n", "");
    let poked = inherits.replace(
        "states = [\"Waiting\", \"Pinged\", \"Stamped\"]",
        "states = [\"Waiting\", \"Pinged\", \"Stamped\", \"Poked\", \"Othered\"]",
    ) + r#"
[[action]]
name = "Poke"
kind = "input"
from = ["Waiting"]
to = "Poked"

[[action.triggers]]
name = "other"
kind = "entity"
target_entity = "Child"
target_action = "Other"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Other"
kind = "internal"
from = ["Poked"]
to = "Othered"
"#;
    let policies = r#"
permit(principal, action == Action::"Ping", resource);
permit(principal, action == Action::"Poke", resource);
permit(principal, action == Action::"Stamp", resource);
permit(principal == Customer::"anonymous", action == Action::"Other", resource);
"#;
    let outcomes = liveness(&poked, policies);
    assert!(
        outcomes
            .iter()
            .any(|outcome| matches!(outcome, LivenessOutcome::Stuck { .. })),
        "{outcomes:#?}"
    );
}

const VOUCHED: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Seconded", "Stamped"]
initial = "Waiting"
strict_action_params = true
[[state]]
name = "second"
type = "string"
initial = ""
[[state]]
name = "stamped_by"
type = "string"
initial = ""
[[action]]
name = "Second"
kind = "input"
from = ["Waiting"]
to = "Seconded"
params = [{ name = "second", type = "string", source = "authenticated_subject" }]
[[action]]
name = "Stamp"
kind = "internal"
from = ["Seconded"]
to = "Stamped"
params = [{ name = "stamped_by", type = "string", source = "authenticated_subject" }]
[[invariant]]
name = "four_eyes"
assert = "status != 'Stamped' || stamped_by != second || Parent[Id].status == 'Waiting'"
"#;
const OPENED: &str = r#"
[automaton]
name = "Parent"
states = ["Waiting", "Opened"]
initial = "Waiting"
strict_action_params = true
[[state]]
name = "opener"
type = "string"
initial = ""
[[action]]
name = "Open"
kind = "input"
from = ["Waiting"]
to = "Opened"
params = [{ name = "opener", type = "string", source = "authenticated_subject" }]
[[action.triggers]]
name = "stamp"
kind = "entity"
target_entity = "Child"
target_action = "Stamp"
[action.triggers.resolve_target]
kind = "same_id"
[[invariant]]
name = "opened_by_someone"
assert = "status != 'Opened' || opener != ''"
"#;
const TWO_IDS: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Stamped"]
initial = "Waiting"
strict_action_params = true
[[state]]
name = "x"
type = "string"
initial = ""
[[state]]
name = "y"
type = "string"
initial = ""
[[action]]
name = "Stamp"
kind = "internal"
from = ["Waiting"]
to = "Stamped"
params = [{ name = "x", type = "string", source = "authenticated_subject" }, { name = "y", type = "string", source = "authenticated_subject" }]
[[invariant]]
name = "one_caller"
assert = "status != 'Stamped' || x == y || Parent[Id].status == 'Open'"
"#;
const STARTED: &str = r#"
[automaton]
name = "Doc"
states = ["Idle", "Running", "Done"]
initial = "Idle"
strict_action_params = true
[[state]]
name = "a"
type = "string"
initial = ""
[[state]]
name = "c"
type = "string"
initial = ""
[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Running"
params = [{ name = "a", type = "string", source = "authenticated_subject" }]
[[action.triggers]]
name = "work"
kind = "wasm"
module = "work"
on_success = "Finish"
[[action.triggers]]
name = "touch"
kind = "entity"
target_entity = "Parent"
target_action = "Touch"
[action.triggers.resolve_target]
kind = "same_id"
[[action]]
name = "Finish"
kind = "internal"
from = ["Running"]
to = "Done"
params = [{ name = "c", type = "string", source = "authenticated_subject" }]
[[invariant]]
name = "finisher_is_starter"
assert = "status != 'Done' || c == a || Parent[Id].status == 'Closed'"
"#;
const TOUCHED: &str = r#"
[automaton]
name = "Parent"
states = ["Open", "Closed"]
initial = "Open"
[[action]]
name = "Touch"
kind = "input"
from = ["Open"]
to = "Open"
[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"
guard = "false"
"#;

/// Agents only, with a move the kernel makes itself refused to a request.
fn kernel_only(action: &str) -> String {
    format!(
        "permit(principal is Agent, action, resource) unless {{ action == Action::\"{action}\" }};\n\
         permit(principal is Agent, action == Action::\"{action}\", resource) when {{ context has via && context.via != \"request\" }};"
    )
}

/// The caller a chain carries is the one a request of the same agent binds,
/// whatever else reads the field it was first bound into.
#[test]
fn a_caller_carried_along_a_chain_meets_the_same_caller_elsewhere() {
    assert!(!holds_under(&[OPENED, VOUCHED], &kernel_only("Stamp")));
}

/// Every parameter bound to one source gets the same caller.
#[test]
fn two_parameters_bound_to_one_source_hold_one_caller() {
    let ping = PARENT;
    assert!(holds_under(&[ping, TWO_IDS], &kernel_only("Stamp")));
}

/// A module's answer runs as its call's sender, so it binds the caller who
/// started the call.
#[test]
fn a_modules_answer_binds_the_caller_who_started_its_call() {
    assert!(holds_under(&[STARTED, TOUCHED], &kernel_only("Finish")));
}

const MARKED: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Stamped"]
initial = "Waiting"
strict_action_params = true
[[state]]
name = "noted"
type = "string"
initial = ""
[[state]]
name = "zmarked"
type = "string"
initial = ""
[[state]]
name = "blocked"
type = "string"
initial = ""
[[state]]
name = "stamped_by"
type = "string"
initial = ""
[[action]]
name = "Note"
kind = "input"
from = ["Waiting"]
to = "Waiting"
guard = "noted == ''"
params = [{ name = "noted", type = "string", source = "authenticated_subject" }]
[[action]]
name = "Mark"
kind = "input"
from = ["Waiting"]
to = "Waiting"
guard = "noted != '' && zmarked == ''"
params = [{ name = "zmarked", type = "string", source = "authenticated_subject" }]
[[action.constraints]]
kind = "param_not_equals_field"
param = "zmarked"
field = "noted"
[[action]]
name = "Stamp"
kind = "internal"
from = ["Waiting"]
to = "Stamped"
guard = "noted != ''"
params = [{ name = "a_who", type = "string", source = "authenticated_subject" }, { name = "stamped_by", type = "string", source = "authenticated_subject" }]
[[action.constraints]]
kind = "param_not_equals_field"
param = "a_who"
field = "blocked"
[[invariant]]
name = "stamper_is_not_marker"
assert = "status != 'Stamped' || stamped_by != zmarked || Parent[Id].status == 'Open'"
[[invariant]]
name = "blocked_stays"
assert = "blocked == ''"
"#;
const PINGED: &str = r#"
[automaton]
name = "Parent"
states = ["Open", "Pinged"]
initial = "Open"
[[action]]
name = "Ping"
kind = "input"
from = ["Open"]
to = "Pinged"
[[action.triggers]]
name = "stamp"
kind = "entity"
target_entity = "Child"
target_action = "Stamp"
[action.triggers.resolve_target]
kind = "same_id"
"#;
const ARMED: &str = r#"
[automaton]
name = "Doc"
states = ["Idle", "Armed", "Running", "Done"]
initial = "Idle"
strict_action_params = true
[[state]]
name = "armer"
type = "string"
initial = ""
[[state]]
name = "c"
type = "string"
initial = ""
[[action]]
name = "Arm"
kind = "input"
from = ["Idle"]
to = "Armed"
params = [{ name = "armer", type = "string", source = "authenticated_subject" }]
[[action]]
name = "Start"
kind = "internal"
from = ["Armed"]
to = "Running"
[[action.triggers]]
name = "work"
kind = "wasm"
module = "work"
on_success = "Finish"
[[action]]
name = "Finish"
kind = "internal"
from = ["Running"]
to = "Done"
params = [{ name = "c", type = "string", source = "authenticated_subject" }]
[[state_timeout]]
state = "Armed"
after_seconds = 60
on_timeout = "Start"
[[invariant]]
name = "finisher_is_armer"
assert = "status != 'Done' || c == armer || Parent[Id].status == 'Closed'"
"#;
const ANSWERED: &str = r#"
[automaton]
name = "Doc"
states = ["Idle", "Running", "Done"]
initial = "Idle"
strict_action_params = true
[[state]]
name = "c"
type = "string"
initial = ""
[[action]]
name = "Start"
kind = "input"
from = ["Idle"]
to = "Running"
[[action.triggers]]
name = "work"
kind = "wasm"
module = "work"
on_success = "Finish"
[[action]]
name = "Finish"
kind = "internal"
from = ["Running"]
to = "Done"
params = [{ name = "c", type = "string", source = "authenticated_subject" }]
[[action.triggers]]
name = "mark"
kind = "entity"
target_entity = "Parent"
target_action = "Mark"
[action.triggers.resolve_target]
kind = "same_id"
[[invariant]]
name = "one_caller"
assert = "Parent[Id].status != 'Marked' || c == Parent[Id].m"
"#;
const MARKS: &str = r#"
[automaton]
name = "Parent"
states = ["Open", "Marked"]
initial = "Open"
strict_action_params = true
[[state]]
name = "m"
type = "string"
initial = ""
[[action]]
name = "Mark"
kind = "internal"
from = ["Open"]
to = "Marked"
params = [{ name = "m", type = "string", source = "authenticated_subject" }]
"#;

/// A request binds one caller into every parameter bound to one source too.
#[test]
fn a_request_binds_one_caller_into_each_parameter_of_a_source() {
    let requested = TWO_IDS.replace("kind = \"internal\"", "kind = \"input\"");
    assert!(holds_under(
        &[PARENT, &requested],
        "permit(principal is Agent, action, resource);"
    ));
}

/// A parameter of a source no rule reads still leaves the source's other
/// parameters every caller, not the one value it was cut to.
#[test]
fn a_parameter_no_one_reads_does_not_fix_its_sources_caller() {
    assert!(!holds_under(&[PINGED, MARKED], POLICIES_MARKED));
}

/// A timer's chain is not known to the walk, so a module's answer in it
/// binds a caller the walk cannot name: it says nothing, never breaks.
#[test]
fn an_answer_in_a_chain_the_walk_cannot_name_breaks_nothing() {
    let outcomes = outcomes_under(&[ARMED, TOUCHED], POLICIES_ARMED);
    assert!(
        !outcomes
            .iter()
            .any(|outcome| matches!(outcome, JointOutcome::Broken { .. })),
        "{outcomes:#?}"
    );
}

/// What a module's answer binds, the reactions it sets off bind too.
#[test]
fn an_answers_binding_carries_on_along_its_chain() {
    assert!(holds_under(&[ANSWERED, MARKS], POLICIES_ANSWERED));
}

/// A bound parameter no rule reads, alone in its move, still takes every
/// caller its constraints allow: the reaction it sets off binds the same one.
#[test]
fn a_lone_bound_parameter_no_one_reads_carries_every_caller() {
    assert!(!holds_under(&[LONE_PINGED, LONE_MARKED], POLICIES_MARKED));
}

const LONE_PINGED: &str = r#"
[automaton]
name = "Parent"
states = ["Open", "Pinged"]
initial = "Open"
strict_action_params = true
[[state]]
name = "blocked"
type = "string"
initial = ""
[[action]]
name = "Ping"
kind = "input"
from = ["Open"]
to = "Pinged"
guard = "Child[Id].status == 'Marked'"
params = [{ name = "a_who", type = "string", source = "authenticated_subject" }]
[[action.constraints]]
kind = "param_not_equals_field"
param = "a_who"
field = "blocked"
[[action.triggers]]
name = "stamp"
kind = "entity"
target_entity = "Child"
target_action = "Stamp"
[action.triggers.resolve_target]
kind = "same_id"
[[invariant]]
name = "blocked_stays"
assert = "blocked == ''"
"#;
const LONE_MARKED: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Marked", "Stamped"]
initial = "Waiting"
strict_action_params = true
[[state]]
name = "noted"
type = "string"
initial = ""
[[state]]
name = "zmarked"
type = "string"
initial = ""
[[state]]
name = "blocked"
type = "string"
initial = ""
[[state]]
name = "stamped_by"
type = "string"
initial = ""
[[action]]
name = "Note"
kind = "input"
from = ["Waiting"]
to = "Waiting"
guard = "noted == ''"
params = [{ name = "noted", type = "string", source = "authenticated_subject" }]
[[action]]
name = "Mark"
kind = "input"
from = ["Waiting"]
to = "Marked"
guard = "noted != '' && zmarked == ''"
params = [{ name = "zmarked", type = "string", source = "authenticated_subject" }]
[[action.constraints]]
kind = "param_not_equals_field"
param = "zmarked"
field = "noted"
[[action]]
name = "Stamp"
kind = "internal"
from = ["Marked"]
to = "Stamped"
guard = "noted != ''"
params = [{ name = "stamped_by", type = "string", source = "authenticated_subject" }]
[[invariant]]
name = "stamper_is_not_marker"
assert = "status != 'Stamped' || stamped_by != zmarked || Parent[Id].status == 'Open'"
"#;
const POLICIES_MARKED: &str = r#"
permit(principal is Agent, action, resource) unless { action == Action::"Stamp" };
permit(principal is Agent, action == Action::"Stamp", resource) when { context has via && context.via != "request" };
"#;
const POLICIES_ARMED: &str = r#"
permit(principal is Agent, action, resource) unless { action in [Action::"Start", Action::"Finish"] };
permit(principal is Agent, action in [Action::"Start", Action::"Finish"], resource) when { context has via && context.via != "request" };
"#;
const POLICIES_ANSWERED: &str = r#"
permit(principal is Agent, action, resource) unless { action in [Action::"Finish", Action::"Mark"] };
permit(principal is Agent, action in [Action::"Finish", Action::"Mark"], resource) when { context has via && context.via != "request" };
"#;
