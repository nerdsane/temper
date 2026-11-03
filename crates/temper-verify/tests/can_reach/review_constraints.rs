//! Regressions for routes certified through impossible input constraints and
//! valid routes lost when an input must equal the record's status.

use std::collections::BTreeMap;

use serde_json::json;
use temper_jit::{EvalContext, TransitionTable};

use super::{TOKEN, broken, model_check};

fn assert_unproved(spec: &str) {
    let l1 = model_check(spec);
    assert!(!l1.passed, "{}", l1.summary);
    let result = l1.verification.as_ref().unwrap();
    assert!(result.is_complete);
    assert!(result.counterexamples.is_empty(), "{result:?}");
    assert_eq!(result.unproved_can_reach.len(), 1, "{result:?}");
    assert!(result.unproved_can_reach[0].contains("can_finish"));
    assert!(result.unproved_can_reach[0].contains("Finish"));
    assert!(result.unproved_can_reach[0].contains("proof"));
}

#[test]
fn impossible_numeric_constraints_cannot_certify_a_route() {
    // Neither an unused input nor one read by an effect can certify an exit.
    for kind in ["counter", "int"] {
        for effect in ["", "effect = [\"observed = params.proof\"]"] {
            let spec = format!(
                "{}\n[[state]]\nname = \"count\"\ntype = {kind:?}\ninitial = 0\n\
                 [[state]]\nname = \"observed\"\ntype = \"counter\"\ninitial = 0\n",
                TOKEN.replace(
                    "guard = \"token == 'ok'\"",
                    &format!(
                        r#"
params = [{{ name = "proof", type = "counter" }}]
{effect}
[[action.constraints]]
kind = "param_equals_field"
param = "proof"
field = "count"
[[action.constraints]]
kind = "param_greater_than_field"
param = "proof"
field = "count"
"#
                    )
                )
            );
            let table = TransitionTable::from_ioa_source(&spec);
            for proof in [0, 1, 2, 3] {
                assert!(
                    table
                        .validate_action_params(
                            "Finish",
                            &json!({"proof": proof}),
                            &json!({}),
                            &BTreeMap::new(),
                            &BTreeMap::new(),
                        )
                        .is_err()
                );
            }
            assert_unproved(&spec);
            // Production accepts the equality-only control. Until these
            // constraints are modeled, L1 must still call it unproved.
            let control = spec.replace("param_greater_than_field", "param_equals_field");
            let table = TransitionTable::from_ioa_source(&control);
            assert!(
                table
                    .validate_action_params(
                        "Finish",
                        &json!({"proof": 0}),
                        &json!({}),
                        &BTreeMap::new(),
                        &BTreeMap::new(),
                    )
                    .is_ok()
            );
            assert_unproved(&control);
        }
    }
}

#[test]
fn impossible_boolean_constraints_cannot_certify_a_route() {
    let spec = format!(
        "{}{}",
        TOKEN.replace(
            "guard = \"token == 'ok'\"",
            r#"
params = [{ name = "proof", type = "bool" }]
[[action.constraints]]
kind = "param_equals_field"
param = "proof"
field = "no"
[[action.constraints]]
kind = "param_equals_field"
param = "proof"
field = "yes"
"#,
        ),
        r#"
[[state]]
name = "no"
type = "bool"
initial = false
[[state]]
name = "yes"
type = "bool"
initial = true
"#
    );
    let table = TransitionTable::from_ioa_source(&spec);
    for proof in [false, true] {
        assert!(
            table
                .validate_action_params(
                    "Finish",
                    &json!({"proof": proof}),
                    &json!({}),
                    &BTreeMap::new(),
                    &BTreeMap::new(),
                )
                .is_err()
        );
    }
    assert_unproved(&spec);
    let control = spec.replace("field = \"yes\"", "field = \"no\"");
    let table = TransitionTable::from_ioa_source(&control);
    assert!(
        table
            .validate_action_params(
                "Finish",
                &json!({"proof": false}),
                &json!({}),
                &BTreeMap::new(),
                &BTreeMap::new(),
            )
            .is_ok()
    );
    assert_unproved(&control);
}

#[test]
fn a_string_input_can_equal_a_later_status_without_a_literal_hint() {
    let spec = r#"
[automaton]
name = "StatusInput"
states = ["Start", "Waiting", "Done"]
initial = "Start"
[[state]]
name = "token"
type = "string"
initial = ""
[[action]]
name = "Supply"
from = ["Start"]
to = "Waiting"
params = [{ name = "token", type = "string" }]
[[action]]
name = "Finish"
from = ["Waiting"]
to = "Done"
guard = "token == status"
[[liveness]]
name = "can_finish"
from = ["Start"]
can_reach = ["Done"]
"#;
    let table = TransitionTable::from_ioa_source(spec);
    assert!(
        table
            .validate_action_params(
                "Supply",
                &json!({"token": "Waiting"}),
                &json!({}),
                &BTreeMap::new(),
                &BTreeMap::new(),
            )
            .is_ok()
    );
    let ctx = EvalContext {
        fields: [("token".into(), json!("Waiting"))].into(),
        ..Default::default()
    };
    assert!(
        table
            .evaluate_ctx("Waiting", &ctx, "Finish")
            .unwrap()
            .success
    );
    let l1 = model_check(spec);
    assert!(l1.passed, "{}", l1.summary);
    let result = l1.verification.as_ref().unwrap();
    assert!(result.is_complete);
    assert!(result.bounded_by.is_empty());
    assert!(result.unproved_can_reach.is_empty());
    // The same rule still refuses a real mismatch.
    let mismatch = spec.replace("token == status", "token == status && empty(token)");
    broken(&model_check(&mismatch), "can_finish");
}
