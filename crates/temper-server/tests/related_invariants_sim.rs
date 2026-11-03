//! Related-status invariants must see the same records in simulation as dispatch.
use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{
    FaultConfig, SimActorSystem, SimActorSystemConfig, SimIntegrationResponses,
};
use temper_server::entity_actor::EntityActorHandler;

const PARENT: &str = r#"
[automaton]
name = "Parent"
states = ["Open", "Closed"]
initial = "Open"

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[action]]
name = "Reopen"
kind = "input"
from = ["Closed"]
to = "Open"
"#;

const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["Draft", "Submitted", "Done"]
initial = "Draft"

[[state]]
name = "parent_id"
type = "string"
initial = ""

[[action]]
name = "Link"
kind = "input"
from = ["Draft"]
params = ["parent_id"]

[[action]]
name = "Submit"
kind = "input"
from = ["Draft"]
to = "Submitted"

[[action]]
name = "Complete"
kind = "input"
from = ["Submitted"]
to = "Done"
guard = "Parent[parent_id].status == 'Open'"

[[invariant]]
name = "SubmittedUnderOpenParent"
assert = "status == 'Submitted' => Parent[parent_id].status == 'Open'"
"#;

fn simulation(seed: u64) -> SimActorSystem {
    let mut sim = SimActorSystem::new(SimActorSystemConfig {
        seed,
        max_ticks: 200,
        faults: FaultConfig::none(),
        max_actions_per_actor: 40,
    });
    // Scheduler IDs deliberately differ from entity IDs. Lookup is typed.
    sim.register_actor(
        "parent-actor",
        Box::new(EntityActorHandler::new(
            "Parent",
            "p1",
            Arc::new(TransitionTable::from_ioa_source(PARENT)),
        )),
    );
    sim.register_actor(
        "child-actor",
        Box::new(EntityActorHandler::new(
            "Child",
            "c1",
            Arc::new(TransitionTable::from_ioa_source(CHILD)),
        )),
    );
    sim
}

#[test]
fn related_invariant_sim_uses_current_parent_status_and_preserves_refused_moves() {
    let mut sim = simulation(0);
    sim.step("child-actor", "Link", r#"{"parent_id":"p1"}"#)
        .unwrap();
    sim.step("parent-actor", "Close", "{}").unwrap();
    let before = sim.events_json("child-actor");
    let error = sim.step("child-actor", "Submit", "{}").unwrap_err();
    assert!(error.contains("SubmittedUnderOpenParent"), "{error}");
    sim.assert_status("child-actor", "Draft");
    assert_eq!(sim.events_json("child-actor"), before);

    sim.step("parent-actor", "Reopen", "{}").unwrap();
    sim.step("child-actor", "Submit", "{}").unwrap();
    sim.assert_status("child-actor", "Submitted");
    sim.step("child-actor", "Complete", "{}").unwrap();
    sim.assert_status("child-actor", "Done");
}

#[test]
fn related_invariant_sim_missing_parent_still_refuses() {
    let mut sim = simulation(0);
    sim.step("child-actor", "Link", r#"{"parent_id":"missing"}"#)
        .unwrap();
    let error = sim.step("child-actor", "Submit", "{}").unwrap_err();
    assert!(error.contains("SubmittedUnderOpenParent"), "{error}");
    sim.assert_status("child-actor", "Draft");
}

#[test]
fn related_invariant_sim_random_workload_reaches_actions_after_submission() {
    let mut completed = 0;
    for seed in 0..64 {
        let run = || {
            let mut sim = simulation(seed);
            sim.step("child-actor", "Link", r#"{"parent_id":"p1"}"#)
                .unwrap();
            sim.run_random_recorded()
        };
        let (result, record) = run();
        assert_eq!(record, run().1, "seed {seed}: replay differs");
        assert!(
            result.all_invariants_held,
            "seed {seed}: {:?}",
            result.violations
        );
        let mut parent_status = "Open";
        for (_, id, action, _, to) in &record.transitions {
            if id == "parent-actor" {
                parent_status = to;
            } else if id == "child-actor" && action == "Submit" {
                assert_eq!(
                    parent_status, "Open",
                    "seed {seed}: submitted under closed parent"
                );
            }
        }
        completed += record
            .transitions
            .iter()
            .filter(|(_, id, action, _, _)| id == "child-actor" && action == "Complete")
            .count();
    }
    assert!(completed > 0, "no child reached Complete across 64 seeds");
}

#[test]
fn related_invariant_sim_callbacks_see_the_resolved_parent() {
    let mut sim = simulation(0);
    let callback_spec = CHILD.replace(
        "to = \"Submitted\"",
        "to = \"Submitted\"\n\n[[action.triggers]]\nname = \"notify\"\nkind = \"hook\"\nhook = \"notify\"",
    );
    sim.register_actor(
        "callback-child",
        Box::new(EntityActorHandler::new(
            "Child",
            "c2",
            Arc::new(TransitionTable::from_ioa_source(&callback_spec)),
        )),
    );
    sim.set_integration_responses(SimIntegrationResponses::new().on_trigger(
        "callback-child",
        "notify",
        "Complete",
    ));
    sim.step("callback-child", "Link", r#"{"parent_id":"p1"}"#)
        .unwrap();
    sim.step("callback-child", "Submit", "{}").unwrap();
    sim.assert_status("callback-child", "Done");
}
