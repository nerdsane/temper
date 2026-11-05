use super::*;

/// A ping starts its job by a reaction; the job's Start finishes it by
/// another. Only Job is in the rule's scope, so Start arrives from outside.
const PING: &str = r#"
[automaton]
name = "Ping"
states = ["Unmade", "Sent"]
initial = "Unmade"

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Sent"
params = []

[[action.triggers]]
name = "start_job"
kind = "entity"
target_entity = "Job"
target_action = "Start"
resolve_target = { kind = "same_id" }
"#;

const JOB: &str = r#"
[automaton]
name = "Job"
states = ["Unmade", "Started", "Done"]
initial = "Unmade"

[[action]]
name = "Start"
kind = "internal"
from = ["Unmade"]
to = "Started"
params = []

[[action.triggers]]
name = "finish"
kind = "entity"
target_entity = "Job"
target_action = "Finish"
resolve_target = { kind = "same_id" }

[[action]]
name = "Finish"
kind = "input"
from = ["Started"]
to = "Done"
params = []

[[liveness]]
name = "started_job_finishes"
eventually = "status != 'Started'"
"#;

fn check(policies: &str) -> JointLivenessResult {
    check_among(&[PING, JOB], policies)
}

fn check_among(sources: &[&str], policies: &str) -> JointLivenessResult {
    let automatons: Vec<Automaton> = sources
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
    let result = results.remove(0);
    assert_eq!(result.scope, ["Job"], "{result}");
    result
}

#[test]
fn an_outside_reaction_runs_as_whoever_may_send_the_move_that_set_it_off() {
    // Only a member may Add a ping, and a member may Finish: every Start
    // that arrives is followed by a Finish Cedar allows.
    let result = check(
        r#"
permit(principal, action == Action::"Add", resource is Ping)
when { principal has role && principal.role == "member" };
permit(principal, action == Action::"Start", resource is Job)
when { context has via && context.via != "request" };
permit(principal, action == Action::"Finish", resource is Job)
when { principal has role && principal.role == "member" };
"#,
    );
    assert!(result.holds(), "{result}");
}

#[test]
fn an_outside_reaction_cedar_refuses_its_sender_never_arrives() {
    // Anyone may Add a ping, but only a member may Start or Finish: a
    // stranger's ping is refused its Start, so no job starts unfinished.
    let result = check(
        r#"
permit(principal, action == Action::"Add", resource is Ping);
permit(principal, action in [Action::"Start", Action::"Finish"], resource is Job)
when { principal has role && principal.role == "member" };
"#,
    );
    assert!(result.holds(), "{result}");
}

#[test]
fn a_stranger_who_starts_the_chain_outside_the_scope_is_refused_the_finish() {
    // Anyone may Add a ping and any reaction may Start, but only a member
    // may Finish: a stranger's job starts and stays Started.
    let result = check(
        r#"
permit(principal, action == Action::"Add", resource is Ping);
permit(principal, action == Action::"Start", resource is Job)
when { context has via && context.via != "request" };
permit(principal, action == Action::"Finish", resource is Job)
when { principal has role && principal.role == "member" };
"#,
    );
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{result}"
    );
}

/// A pings B, and B's module answer starts the job: Ping in another dress,
/// with a module between.
const A: &str = r#"
[automaton]
name = "A"
states = ["Unmade", "Sent"]
initial = "Unmade"

[[action]]
name = "Add"
kind = "input"
from = ["Unmade"]
to = "Sent"
params = []

[[action.triggers]]
name = "to_begin"
kind = "entity"
target_entity = "B"
target_action = "Begin"
resolve_target = { kind = "same_id" }
"#;

const B: &str = r#"
[automaton]
name = "B"
states = ["Unmade", "Begun", "Answered"]
initial = "Unmade"

[[action]]
name = "Begin"
kind = "internal"
from = ["Unmade"]
to = "Begun"
params = []

[[action.triggers]]
name = "ask"
kind = "wasm"
module = "asker"
on_success = "Answer"

[[action]]
name = "Answer"
kind = "internal"
from = ["Begun"]
to = "Answered"
params = []

[[action.triggers]]
name = "to_start"
kind = "entity"
target_entity = "Job"
target_action = "Start"
resolve_target = { kind = "same_id" }
"#;

#[test]
fn a_chain_through_a_module_answer_runs_as_whoever_started_it_outside() {
    // Anyone may Add an A, and no one may ask B anything: a stranger's A
    // still reaches Start through B's module, and the stranger may not
    // Finish.
    let result = check_among(
        &[A, B, JOB],
        r#"
permit(principal, action == Action::"Add", resource is A);
permit(principal, action == Action::"Begin", resource is B) when { context has via && context.via != "request" };
permit(principal, action == Action::"Start", resource is Job) when { context has via && context.via != "request" };
permit(principal, action == Action::"Finish", resource is Job) when { principal has role && principal.role == "member" };
"#,
    );
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{result}"
    );
}

#[test]
fn a_sender_who_differs_only_on_the_start_is_kept_apart() {
    // Members and helpers may Add a ping, and only members may Finish: a
    // helper's job starts and stays Started.
    let result = check(
        r#"
permit(principal, action == Action::"Add", resource is Ping) when { principal has role && (principal.role == "member" || principal.role == "helper") };
permit(principal, action == Action::"Start", resource is Job) when { context has via && context.via != "request" };
permit(principal, action == Action::"Finish", resource is Job) when { principal has role && principal.role == "member" };
"#,
    );
    assert!(
        matches!(result.outcome, LivenessOutcome::Stuck { .. }),
        "{result}"
    );
}

/// A member mints a job, which the kernel starts.
const MINT: &str = r#"
[automaton]
name = "Mint"
states = ["Open", "Made"]
initial = "Open"

[[state]]
name = "job"
type = "string"
initial = ""

[[action]]
name = "Make"
kind = "input"
from = ["Open"]
to = "Made"
effect = ["spawn('Job', 'Start', job)"]
"#;

#[test]
fn only_a_spawn_skips_cedar_on_a_start_a_reaction_also_sends() {
    // The kernel starts a minted job whatever Cedar says, and only members
    // mint; a stranger's ping sends Start too, which Cedar refuses everyone.
    let result = check_among(
        &[PING, JOB, MINT],
        r#"
permit(principal, action == Action::"Make", resource is Mint) when { principal has role && principal.role == "member" };
permit(principal, action == Action::"Add", resource is Ping);
permit(principal, action == Action::"Finish", resource is Job) when { principal has role && principal.role == "member" };
"#,
    );
    assert!(result.holds(), "{result}");
}
