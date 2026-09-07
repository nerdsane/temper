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
    {
        let mut sim = simulation(0, CONTRACT);
        let before = sim.events_json("resource");
        let result = sim.step(
            "resource",
            "Observe",
            r#"{"observed":"release-b","desired":"release-b"}"#,
        );
        assert!(result.is_err(), "observation changed desired configuration");
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
    for malformed in ["{", "null", "[]", "42", "\"text\""] {
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

#[test]
fn fresh_defaults_and_every_constraint_run_through_actual_actor() {
    let source = CONTRACT
        .replacen(
            "[[action]]",
            r#"
[[state]]
name = "sequence"
type = "counter"
initial = "0"
[[state]]
name = "enabled"
type = "bool"
initial = "false"
[[action]]"#,
            1,
        )
        .replace(
            "params = [\"observed\"]",
            r#"
params = ["observed", "expected_sequence", "next_sequence", "expected_enabled"]
[[action.constraints]]
kind = "param_equals_field"
param = "expected_sequence"
field = "sequence"
[[action.constraints]]
kind = "param_greater_than_field"
param = "next_sequence"
field = "sequence"
[[action.constraints]]
kind = "param_equals_field"
param = "expected_enabled"
field = "enabled"
[[action.constraints]]
kind = "param_not_equals_field"
param = "observed"
field = "observed"
[[action.constraints]]
kind = "param_nonempty"
param = "observed"
"#,
        );
    let mut sim = simulation(0, &source);
    let before = sim.events_json("resource");
    let good = serde_json::json!({"observed":"release-b","expected_sequence":0,"next_sequence":1,"expected_enabled":false});
    for (key, invalid) in [
        ("expected_sequence", serde_json::json!("0")),
        ("next_sequence", serde_json::json!(0)),
        ("next_sequence", serde_json::json!("1")),
        ("next_sequence", serde_json::json!(1.0)),
        ("expected_enabled", serde_json::json!("false")),
        ("observed", serde_json::json!("")),
        ("observed", serde_json::json!("   ")),
    ] {
        let mut params = good.clone();
        params[key] = invalid;
        assert!(
            sim.step("resource", "Observe", &params.to_string())
                .is_err()
        );
        assert_eq!(before, sim.events_json("resource"));
    }
    let after = sim.step("resource", "Observe", &good.to_string()).unwrap();
    assert_eq!(after["fields"]["desired"], "release-a");
    assert_eq!(after["fields"]["observed"], "release-b");
    let events = sim.events_json("resource");
    assert!(sim.step("resource", "Observe", &good.to_string()).is_err());
    assert_eq!(events, sim.events_json("resource"));
}

#[test]
fn inequality_rejects_invalid_numeric_representations() {
    let source = CONTRACT
        .replacen(
            "[[action]]",
            "[[state]]\nname = \"sequence\"\ntype = \"counter\"\ninitial = \"0\"\n[[action]]",
            1,
        )
        .replace(
            "params = [\"observed\"]",
            r#"params = ["next_sequence"]
[[action.constraints]]
kind = "param_not_equals_field"
param = "next_sequence"
field = "sequence"
"#,
        );
    let mut sim = simulation(0, &source);
    for invalid in [
        serde_json::json!("0"),
        serde_json::json!(1.0),
        serde_json::json!(false),
        serde_json::json!(null),
        serde_json::json!(0),
    ] {
        assert!(
            sim.step(
                "resource",
                "Observe",
                &serde_json::json!({"next_sequence":invalid}).to_string()
            )
            .is_err()
        );
    }
    sim.step("resource", "Observe", r#"{"next_sequence":1}"#)
        .unwrap();
}

#[test]
fn nonzero_defaults_are_used_by_guards_and_constraints() {
    let source = CONTRACT
        .replacen(
            "[[action]]",
            "[[state]]\nname = \"sequence\"\ntype = \"counter\"\ninitial = \"3\"\n[[action]]",
            1,
        )
        .replace(
            "params = [\"observed\"]",
            r#"params = ["expected_sequence"]
guard = "sequence >= 3"
[[action.constraints]]
kind = "param_equals_field"
param = "expected_sequence"
field = "sequence"
"#,
        );
    let mut sim = simulation(0, &source);
    sim.step("resource", "Observe", r#"{"expected_sequence":3}"#)
        .unwrap();
}

#[test]
fn signed_integer_field_can_compare_with_its_declared_default() {
    let source = CONTRACT
        .replacen(
            "[[action]]",
            "[[state]]\nname = \"offset\"\ntype = \"integer\"\ninitial = \"-1\"\n[[action]]",
            1,
        )
        .replace(
            "params = [\"observed\"]",
            r#"params = ["offset"]
[[action.constraints]]
kind = "param_equals_field"
param = "offset"
field = "offset"
"#,
        );
    let mut sim = simulation(0, &source);
    sim.step("resource", "Observe", r#"{"offset":-1}"#).unwrap();
}
