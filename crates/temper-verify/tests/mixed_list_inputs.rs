//! Checking concrete string inputs must preserve numeric guard choices too.

use stateright::Model;
use temper_verify::check_model;
use temper_verify::model::{ParamValue, build_model_from_ioa, semantics::successors};

fn assert_guarded_call_is_explored(numeric_effect: &str) {
    let spec = format!(
        r#"
[automaton]
name = "Mixed"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "roles"
type = "list"
initial = []

[[state]]
name = "scratch"
type = "list"
initial = ["held"]

[[state]]
name = "remaining"
type = "counter"
initial = 1

[[action]]
name = "Grant"
from = ["Active"]
params = ["i", "role"]
guard = "params.i == 10"
effect = ["{numeric_effect}", "append(roles, params.role)"]

[[invariant]]
name = "EmptyRoles"
assert = "len(roles) == 0"

[[invariant]]
name = "Held"
assert = "len(scratch) >= 0 && remaining >= 0"
"#
    );
    let model = build_model_from_ioa(&spec, 2).unwrap();
    let initial = model.init_states().remove(0);
    let choices = successors(&model, &model.transitions[0], &initial);
    assert!(
        choices.iter().any(|(params, next)| {
            params["i"] == ParamValue::Count(10) && next.lists["roles"].len() == 1
        }),
        "the accepted call must be explored: {choices:?}"
    );
    let result = check_model(&model);
    assert!(
        result.counterexamples.iter().any(|c| {
            c.violated.contains(&"EmptyRoles".to_string())
                && c.trace.iter().any(|(_, action)| {
                    action
                        .as_ref()
                        .is_some_and(|a| a.params["i"] == ParamValue::Count(10))
                })
        }),
        "the accepted call breaks EmptyRoles: {result:?}"
    );
}

#[test]
fn a_guarded_out_of_range_index_does_not_hide_a_string_effect() {
    assert_guarded_call_is_explored("remove_at(scratch, params.i)");
}

#[test]
fn a_guarded_clamped_subtraction_does_not_hide_a_string_effect() {
    assert_guarded_call_is_explored("remaining -= params.i");
}
