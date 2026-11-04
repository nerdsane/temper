//! A spawn stores its child's id and starts the child, so rules on what a
//! child does are checked, and a second child over a first is not taken for
//! a sure step unless the first can do nothing more.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::super::super::room::{Bytes, Progress};
use super::super::super::*;

/// `Go` makes a `Run` and `Retry` makes another; Cedar lets neither anyone
/// send `Start`, so only a spawn can start one.
fn job(rule: &str) -> String {
    format!(
        r#"
[automaton]
name = "Job"
states = ["Open", "Going", "Again"]
initial = "Open"

[[state]]
name = "run"
type = "string"
initial = ""

[[action]]
name = "Go"
kind = "input"
from = ["Open"]
to = "Going"
effect = ["spawn('Run', 'Start', run)"]

[[action]]
name = "Retry"
kind = "input"
from = ["Going"]
to = "Again"
effect = ["spawn('Run', 'Start', run)"]
{rule}
"#
    )
}

/// `way_back` is appended to the run: with a move out of `Done` that sends
/// something, a finished run may still do more.
fn run(way_back: &str) -> String {
    format!(
        r#"
[automaton]
name = "Run"
states = ["New", "Going", "Done"]
initial = "New"

[[state]]
name = "parent_id"
type = "string"
initial = ""

[[action]]
name = "Start"
kind = "input"
from = ["New"]
to = "Going"

[[action]]
name = "Finish"
kind = "input"
from = ["Going"]
to = "Done"
{way_back}
"#
    )
}

/// A move out of `Done` that sends something on.
const REOPENS: &str = r#"
[[action]]
name = "Reopen"
kind = "input"
from = ["Done"]
to = "Going"

[[action.triggers]]
name = "tell"
kind = "entity"
target_entity = "Job"
target_action = "Heard"
resolve_target = { kind = "field", id_field = "parent_id" }
"#;

/// Lets people move jobs and runs, never `Start`.
const POLICIES: &str = r#"
permit(principal, action in [Action::"Go", Action::"Retry", Action::"Finish", Action::"Reopen"], resource);
"#;

fn invariants(sources: &[String]) -> String {
    let automatons: Vec<Automaton> = sources
        .iter()
        .map(|source| parse_automaton(source).expect("fixture parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(POLICIES).expect("fixture policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let results = verify_joint_invariants(
        &refs,
        Some(&analysis),
        Bound {
            room: &room,
            progress: &progress,
        },
    );
    assert_eq!(results.len(), 1, "{results:?}");
    results[0].to_string()
}

fn rule(assert: &str) -> String {
    format!("[[invariant]]\nname = \"rule\"\nassert = \"{assert}\"\n")
}

#[test]
fn a_rule_on_what_a_child_does_is_broken_through_the_spawn() {
    // Only a spawn can start a run, and only a started run can finish.
    let text = invariants(&[
        job(&rule("status == 'Going' => Run[run].status != 'Done'")),
        run(""),
    ]);
    assert!(text.contains("BROKEN"), "{text}");
    assert!(text.contains("Job.Go"), "{text}");
    assert!(text.contains("Run.Finish"), "{text}");
}

#[test]
fn a_rule_that_holds_of_a_child_holds_having_walked_it() {
    let text = invariants(&[
        job(&rule("Run[run].status == 'Done' => status != 'Open'")),
        run(""),
    ]);
    assert!(text.contains("HOLDS"), "{text}");
}

#[test]
fn the_kernel_tells_a_child_its_parent() {
    let child = run("").replace(
        "name = \"Finish\"\nkind = \"input\"\nfrom = [\"Going\"]\nto = \"Done\"\n",
        &format!(
            "name = \"Finish\"\nkind = \"input\"\nfrom = [\"Going\"]\nto = \"Done\"\n{}",
            rule("status == 'Done' => Job[parent_id].status != 'Open'")
        ),
    );
    let text = invariants(&[job(""), child]);
    assert!(text.contains("HOLDS"), "{text}");
}

#[test]
fn a_second_child_over_a_finished_one_is_a_sure_step() {
    // A finished run can do nothing more, so the new one takes its place.
    let text = invariants(&[
        job(&rule("status == 'Again' => Run[run].status != 'Going'")),
        run(""),
    ]);
    assert!(text.contains("BROKEN"), "{text}");
    assert!(text.contains("Job.Retry"), "{text}");
}

#[test]
fn a_second_child_over_one_that_may_do_more_is_not_a_sure_step() {
    // The runtime would hold both runs and the model holds one, so a break
    // only this replacement reaches is not a break the model can be sure of.
    let parent = job(&rule("status == 'Again' => Run[run].status != 'Going'"));
    let parent = format!(
        "{parent}\n[[action]]\nname = \"Heard\"\nkind = \"internal\"\nfrom = [\"Going\", \"Again\"]\n"
    );
    let text = invariants(&[parent, run(REOPENS)]);
    assert!(!text.contains("BROKEN"), "{text}");
    assert!(!text.contains("HOLDS"), "{text}");
    assert!(text.contains("NOT CHECKED"), "{text}");
}

#[test]
fn a_spawn_from_outside_the_rule_still_starts_its_child() {
    // The rule reads only Run and Box, so Job is outside its scope; its spawn
    // is the only way to start a run.
    const BOX: &str = r#"
[automaton]
name = "Box"
states = ["Empty"]
initial = "Empty"

[[action]]
name = "Look"
kind = "internal"
from = ["Empty"]
to = "Empty"
"#;
    let child = run("").replace(
        "name = \"Finish\"\nkind = \"input\"\nfrom = [\"Going\"]\nto = \"Done\"\n",
        &format!(
            "name = \"Finish\"\nkind = \"input\"\nfrom = [\"Going\"]\nto = \"Done\"\n{}",
            rule("status == 'Done' => Box[Id].status == 'Full'")
        ),
    );
    let text = invariants(&[job(""), child, BOX.to_string()]);
    assert!(!text.contains("HOLDS"), "{text}");
    assert!(!text.contains("CHECKS NOTHING"), "{text}");
}
