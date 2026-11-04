//! With two records of a type, two writers really race. Each planted race in
//! `tests/race_models` has to come out broken, with the trace of the race,
//! and its fixed twin has to hold for up to two records.

use temper_authz::SendAnalysis;
use temper_spec::automaton::{Automaton, parse_automaton};

use super::room::{Bound, Bytes, Progress};
use super::{JointInvariantResult, JointOutcome, invariant_proofs_holding};

macro_rules! model {
    ($name:literal, $($file:literal),+) => {
        (
            vec![$(include_str!(concat!(
                "../../../tests/race_models/", $name, "/", $file, ".ioa.toml"
            ))
            .to_string()),+],
            include_str!(concat!("../../../tests/race_models/", $name, "/app.cedar")),
        )
    };
}

type Model = (Vec<String>, &'static str);

fn check((sources, policies): &Model, records: usize) -> JointInvariantResult {
    let automatons: Vec<Automaton> = sources
        .iter()
        .map(|source| parse_automaton(source).expect("model parses"))
        .collect();
    let refs: Vec<&Automaton> = automatons.iter().collect();
    let analysis = SendAnalysis::new(policies).expect("policies parse");
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let proofs = invariant_proofs_holding(&refs, Some(&analysis), &room, records);
    let results = proofs[0].check(Bound {
        room: &room,
        progress: &progress,
    });
    results.into_iter().next().expect("the model has a rule")
}

/// `model` with its plant taken out: `from` in source `at` becomes `to`.
fn fixed(model: &Model, at: usize, from: &str, to: &str) -> Model {
    let mut sources = model.0.clone();
    assert!(sources[at].contains(from), "{from} is in the model");
    sources[at] = sources[at].replace(from, to);
    (sources, model.1)
}

fn moves(result: &JointInvariantResult) -> Vec<String> {
    match &result.outcome {
        JointOutcome::Broken { path } => path.iter().filter_map(|s| s.action.clone()).collect(),
        other => panic!("expected broken, got {other:?}: {result}"),
    }
}

fn holds_for_two(model: &Model) {
    let result = check(model, 2);
    assert!(result.holds(), "{result}");
    assert!(
        result
            .to_string()
            .contains("HOLDS for up to 2 records of each type"),
        "{result}"
    );
}

fn slot() -> Model {
    model!("slot", "Rollout", "Slot")
}

#[test]
fn two_rollouts_holding_one_slot_takes_two_records_to_see() {
    assert!(check(&slot(), 1).holds());
    let moves = moves(&check(&slot(), 2));
    let takes = moves.iter().filter(|m| m.contains(".Take(")).count();
    let claims = moves.iter().filter(|m| m.contains(".Claim(")).count();
    assert_eq!((takes, claims), (2, 2), "{moves:#?}");
    assert!(moves.iter().any(|m| m.contains("Rollout#2")), "{moves:#?}");
}

#[test]
fn a_slot_that_refuses_a_second_claim_holds_for_two_rollouts() {
    holds_for_two(&fixed(
        &slot(),
        1,
        r#"from = ["Free", "Held"]"#,
        r#"from = ["Free"]"#,
    ));
}

fn task() -> Model {
    model!("task", "Task", "Worker")
}

#[test]
fn two_workers_taking_one_task_takes_two_records_to_see() {
    assert!(check(&task(), 1).holds());
    let moves = moves(&check(&task(), 2));
    let assigns = moves.iter().filter(|m| m.contains(".Assign(")).count();
    assert_eq!(assigns, 2, "{moves:#?}");
}

#[test]
fn a_task_that_refuses_a_second_worker_holds_for_two_workers() {
    holds_for_two(&fixed(
        &task(),
        0,
        r#"from = ["Open", "Assigned"]"#,
        r#"from = ["Open"]"#,
    ));
}

fn counter() -> Model {
    model!("counter", "Taker", "Tally")
}

#[test]
fn two_moves_racing_on_one_counter_takes_two_records_to_see() {
    assert!(check(&counter(), 1).holds());
    let moves = moves(&check(&counter(), 2));
    let counted = moves.iter().filter(|m| m.contains(".Take(")).count();
    assert_eq!(counted, 2, "{moves:#?}");
}

#[test]
fn a_counter_with_a_guard_holds_for_two_takers() {
    holds_for_two(&fixed(
        &counter(),
        1,
        r#"effect = ["taken += 1"]"#,
        "guard = \"taken < 1\"\neffect = [\"taken += 1\"]",
    ));
}

fn spawn() -> Model {
    model!("spawn", "Alert", "Incident")
}

#[test]
fn recovering_before_the_incident_exists_is_broken_either_way() {
    for records in [1, 2] {
        let moves = moves(&check(&spawn(), records));
        let refused = moves
            .iter()
            .position(|m| m.contains("refused by its record"));
        let arrives = moves.iter().position(|m| m.contains("TakenUp arrives"));
        assert!(refused.is_some() && refused < arrives, "{moves:#?}");
    }
}

#[test]
fn an_incident_that_takes_recover_first_holds_for_two() {
    let from = "name = \"Recover\"\nkind = \"input\"\nfrom = [\"Looking\"]";
    let to = "name = \"Recover\"\nkind = \"input\"\nfrom = [\"Unmade\", \"Looking\"]";
    holds_for_two(&fixed(&spawn(), 1, from, to));
}

fn pass() -> Model {
    model!("pass", "Pass", "Run")
}

/// The pass's `RunEnded` asks which run ended.
fn pass_fixed() -> Model {
    let model = fixed(
        &pass(),
        0,
        "name = \"RunEnded\"\nkind = \"input\"\nfrom = [\"Staging\"]\nto = \"Failed\"\n",
        "name = \"RunEnded\"\nkind = \"input\"\nfrom = [\"Staging\"]\nto = \"Failed\"\nguard = \"run == params.ended\"\nparams = [{ name = \"ended\", type = \"string\" }]\n",
    );
    fixed(
        &model,
        1,
        "target_action = \"RunEnded\"\n",
        "target_action = \"RunEnded\"\n[action.triggers.args]\nended = \"Id\"\n",
    )
}

#[test]
fn an_old_run_ending_fails_the_pass_on_a_new_run() {
    let moves = moves(&check(&pass(), 2));
    let stages = moves.iter().filter(|m| m.contains(".Stage")).count();
    let begins = moves
        .iter()
        .filter(|m| m.contains(".Begin arrives"))
        .count();
    assert_eq!((stages, begins), (2, 2), "{moves:#?}");
    assert!(
        moves.iter().any(|m| m.contains(".RunEnded arrives")),
        "{moves:#?}"
    );
}

#[test]
fn a_pass_that_asks_which_run_ended_holds_for_two_runs() {
    holds_for_two(&pass_fixed());
}

fn late_guard() -> Model {
    model!("lateguard", "Order", "Shelf")
}

/// `Confirm` asks again when it lands, so the order's shelf can't have moved.
fn late_guard_asks_again() -> Model {
    fixed(
        &late_guard(),
        0,
        "to = \"Confirmed\"\n",
        "to = \"Confirmed\"\nguard = \"!empty(shelf) && Shelf[shelf].status == 'Stocked'\"\n",
    )
}

#[test]
fn a_late_guard_reads_the_field_as_its_move_committed() {
    let moves = moves(&check(&late_guard(), 1));
    let moved = moves.iter().filter(|m| m.contains(".Move(")).count();
    assert_eq!(moved, 2, "{moves:#?}");
    assert!(
        moves.iter().any(|m| m.contains("guard holds")),
        "{moves:#?}"
    );
}

#[test]
fn a_late_guard_that_names_the_status_still_reads_it() {
    let model = fixed(
        &late_guard(),
        0,
        "guard = \"!empty(shelf) && Shelf",
        "guard = \"status == 'Placed' && !empty(shelf) && Shelf",
    );
    let moves = moves(&check(&model, 1));
    assert!(
        moves.iter().any(|m| m.contains("guard holds")),
        "{moves:#?}"
    );
}

#[test]
fn a_status_a_late_guard_never_reads_splits_no_state() {
    let result = check(&late_guard_asks_again(), 1);
    assert!(
        !matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
    assert!(result.states <= 1_900, "{result}");
}

/// Four moves send one reaction, and Cedar lets only `Drop`'s through.
fn source_move() -> Model {
    model!("sourcemove", "Ticket", "Board")
}

/// The same moves, under policies that can't tell them apart.
fn source_move_unread(rule: &str) -> Model {
    let mut model = fixed(
        &source_move(),
        0,
        "status == 'Dropped' => Board[board].status != 'Pinged'",
        rule,
    );
    model.1 = r#"permit(principal, action in [Action::"Start", Action::"Keep", Action::"Drop", Action::"Poke", Action::"Prod"], resource);
permit(principal, action == Action::"Ping", resource) when { context has via && context.via != "request" };"#;
    model
}

#[test]
fn a_policy_that_reads_the_source_move_keeps_its_moves_apart() {
    for records in [1, 2] {
        let result = check(&source_move(), records);
        assert!(
            matches!(result.outcome, JointOutcome::Broken { .. }),
            "{result}"
        );
        let moves = moves(&result);
        assert!(moves.iter().any(|m| m.contains(".Drop")), "{moves:#?}");
        assert!(moves.iter().any(|m| m.contains(".Ping")), "{moves:#?}");
    }
}

#[test]
fn a_reaction_walked_as_another_move_still_lands_and_names_its_own_move() {
    let result = check(
        &source_move_unread("status == 'Dropped' => Board[board].status != 'Pinged'"),
        1,
    );
    assert!(
        matches!(result.outcome, JointOutcome::Broken { .. }),
        "{result}"
    );
    let moves = moves(&result);
    assert!(moves.iter().any(|m| m.contains(".Drop")), "{moves:#?}");
}

#[test]
fn moves_nothing_can_tell_apart_are_walked_as_one() {
    let model = source_move_unread("status == 'New' => Board[board].status != 'Pinged'");
    let one = check(&model, 1);
    assert!(one.holds(), "{one}");
    assert!(one.states <= 204, "{one}");
    let two = check(&model, 2);
    assert!(two.holds(), "{two}");
    assert!(two.states <= 43_936, "{two}");
}

#[test]
fn a_spawn_makes_one_child() {
    let result = check(&pass(), 2);
    let JointOutcome::Broken { path } = &result.outcome else {
        panic!("{result}");
    };
    let staged = path
        .iter()
        .find(|step| step.action.as_deref().is_some_and(|m| m.contains(".Stage")))
        .expect("the trace stages");
    assert_eq!(
        staged.state.matches(".Begin").count(),
        1,
        "{}",
        staged.state
    );
}

/// Records that hold the same things in another order are one state: two
/// records of three statuses are six pairs, not nine.
#[test]
fn which_copy_holds_what_is_one_state() {
    let toggle = r#"
[automaton]
name = "Toggle"
states = ["A", "B", "C"]
initial = "A"

[[state]]
name = "other"
type = "string"
initial = ""

[[action]]
name = "Next"
kind = "input"
from = ["A"]
to = "B"

[[action]]
name = "Last"
kind = "input"
from = ["B"]
to = "C"

[[invariant]]
name = "Reads"
assert = "status == 'C' => Toggle[other].status != 'Z' || status == 'C'"
"#;
    let model: Model = (
        vec![toggle.to_string()],
        "permit(principal, action, resource);",
    );
    let one = check(&model, 1);
    let two = check(&model, 2);
    assert!(two.states < one.states * one.states, "{one} {two}");
}
