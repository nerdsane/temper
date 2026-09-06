//! Action contracts run through the same actor and effect code as production.

use std::sync::Arc;

use temper_jit::table::TransitionTable;
use temper_runtime::scheduler::{SimActorSystem, SimActorSystemConfig};
use temper_server::entity_actor::sim_handler::EntityActorHandler;

const CONTRACT: &str = r#"
[automaton]
name = "ObservedResource"
states = ["New", "Ready"]
initial = "New"
strict_action_params = true
allow_indefinite_states = ["New", "Ready"]

[[state]]
name = "desired"
type = "string"
initial = ""

[[state]]
name = "observed"
type = "string"
initial = ""

[[action]]
name = "Register"
kind = "input"
from = ["New"]
to = "Ready"
params = ["desired"]

[[action]]
name = "Observe"
kind = "input"
from = ["Ready"]
params = ["observed"]
"#;

fn simulation(seed: u64, source: &str) -> SimActorSystem {
    let table = TransitionTable::from_ioa_source(source);
    // Persisted tables must retain the same contract after a restart.
    let table: TransitionTable =
        serde_json::from_value(serde_json::to_value(table).unwrap()).unwrap();
    let mut sim = SimActorSystem::new(SimActorSystemConfig {
        seed,
        ..Default::default()
    });
    sim.register_actor(
        "resource",
        Box::new(EntityActorHandler::new(
            "ObservedResource",
            "resource",
            Arc::new(table),
        )),
    );
    sim.step("resource", "Register", r#"{"desired":"release-a"}"#)
        .unwrap();
    sim
}

#[test]
fn strict_action_cannot_change_an_undeclared_field() {
    for seed in 0..64 {
        let mut sim = simulation(seed, CONTRACT);
        let before = sim.events_json("resource");
        let result = sim.step(
            "resource",
            "Observe",
            r#"{"observed":"release-b","desired":"release-b"}"#,
        );
        assert!(
            result.is_err(),
            "seed {seed}: observation changed desired configuration"
        );
        assert_eq!(
            before,
            sim.events_json("resource"),
            "rejected action must not emit an event"
        );
        sim.step("resource", "Observe", r#"{"observed":"release-b"}"#)
            .unwrap();
        sim.assert_status("resource", "Ready");
    }
}

#[test]
fn strict_action_rejects_non_object_input() {
    for malformed in ["null", "[]", "42", "\"text\""] {
        let mut sim = simulation(1, CONTRACT);
        assert!(
            sim.step("resource", "Observe", malformed).is_err(),
            "accepted {malformed}"
        );
    }
}

#[test]
fn compare_and_set_checks_the_value_before_mutation() {
    let source = CONTRACT.replace(
        "params = [\"observed\"]",
        r#"params = ["observed", "expected_desired"]
[[action.constraints]]
kind = "param_equals_field"
param = "expected_desired"
field = "desired"
"#,
    );
    let mut sim = simulation(9, &source);
    assert!(
        sim.step(
            "resource",
            "Observe",
            r#"{"observed":"release-b","expected_desired":"release-c"}"#
        )
        .is_err()
    );
    sim.step(
        "resource",
        "Observe",
        r#"{"observed":"release-b","expected_desired":"release-a"}"#,
    )
    .unwrap();
}

#[test]
fn numeric_constraints_reject_payloads_the_numeric_effect_would_skip() {
    let source = CONTRACT
        .replace(
            "params = [\"observed\"]",
            r#"params = ["observed", "expected_sequence"]
[[action.constraints]]
kind = "param_equals_field"
param = "expected_sequence"
field = "sequence"
"#,
        )
        .replacen(
            "[[action]]",
            "[[state]]\nname = \"sequence\"\ntype = \"counter\"\ninitial = \"0\"\n\n[[action]]",
            1,
        );
    let table = TransitionTable::from_ioa_source(&source);
    let fields = serde_json::json!({});
    let counters = std::collections::BTreeMap::from([("sequence".to_owned(), 0)]);
    for value in [
        serde_json::json!("0"),
        serde_json::json!(0.0),
        serde_json::json!(-1),
        serde_json::json!(null),
    ] {
        assert!(
            table
                .validate_action_params(
                    "Observe",
                    &serde_json::json!({"expected_sequence":value}),
                    &fields,
                    &counters,
                    &Default::default()
                )
                .is_err()
        );
    }
    assert!(
        table
            .validate_action_params(
                "Observe",
                &serde_json::json!({"expected_sequence":0}),
                &fields,
                &counters,
                &Default::default()
            )
            .is_ok()
    );
}
