//! Removing a chosen value must explore held values and concrete fresh inputs.

use stateright::Model;
use temper_verify::check_model;
use temper_verify::model::{ModelString, ParamValue, build_model_from_ioa, semantics::successors};

const ROLES: &str = r#"
[automaton]
name = "Role"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "roles"
type = "list"
initial = ["Cook"]

[[action]]
name = "Revoke"
from = ["Active"]
params = ["role"]
effect = ["remove(roles, params.role)"]

[[invariant]]
name = "HasRole"
assert = "len(roles) > 0"
"#;

#[test]
fn removing_an_initial_role_breaks_a_length_only_invariant() {
    let model = build_model_from_ioa(ROLES, 2).unwrap();
    let result = check_model(&model);
    let counterexample = result
        .counterexamples
        .iter()
        .find(|c| c.violated.contains(&"HasRole".to_string()))
        .expect("revoking Cook must violate HasRole");
    assert!(counterexample.trace.last().unwrap().0.lists["roles"].is_empty());
    assert!(counterexample.trace.iter().any(|(_, action)| {
        action
            .as_ref()
            .is_some_and(|a| a.params["role"] == ParamValue::Str("Cook".into()))
    }));
}

#[test]
fn revocation_explores_held_symbols_and_a_concrete_absent_value() {
    let model = build_model_from_ioa(ROLES, 2).unwrap();
    let mut state = model.init_states().remove(0);
    state
        .lists
        .insert("roles".into(), vec![ModelString::Symbol(1)]);
    let choices = successors(&model, &model.transitions[0], &state);
    assert!(
        choices
            .iter()
            .all(|(params, _)| matches!(params["role"], ParamValue::Str(_)))
    );
    assert!(
        choices.iter().any(|(params, next)| {
            params["role"] == ParamValue::Str(ModelString::Symbol(1))
                && next.lists["roles"].is_empty()
        }),
        "{choices:?}"
    );
    assert!(
        choices.iter().any(|(params, next)| {
            params["role"] == ParamValue::Str(ModelString::Symbol(2))
                && next.lists["roles"] == state.lists["roles"]
        }),
        "{choices:?}"
    );
}

#[test]
fn a_value_appended_earlier_in_the_move_can_be_removed() {
    let spec = ROLES
        .replace("initial = [\"Cook\"]", "initial = []")
        .replace(
            "remove(roles, params.role)",
            "append(roles, 'Cook')\", \"remove(roles, params.role)",
        )
        .replace("len(roles) > 0", "len(roles) == 1");
    let model = build_model_from_ioa(&spec, 2).unwrap();
    let state = model.init_states().remove(0);
    let choices = successors(&model, &model.transitions[0], &state);
    assert!(
        choices.iter().any(|(params, next)| {
            params["role"] == ParamValue::Str("Cook".into()) && next.lists["roles"].is_empty()
        }),
        "{choices:?}"
    );
}

#[test]
fn the_same_input_is_used_by_append_and_remove() {
    let spec = ROLES
        .replace("initial = [\"Cook\"]", "initial = []")
        .replace(
            "remove(roles, params.role)",
            "append(roles, params.role)\", \"remove(roles, params.role)",
        )
        .replace("len(roles) > 0", "len(roles) == 0");
    let model = build_model_from_ioa(&spec, 2).unwrap();
    let state = model.init_states().remove(0);
    let choices = successors(&model, &model.transitions[0], &state);
    assert!(!choices.is_empty());
    assert!(
        choices
            .iter()
            .all(|(_, next)| next.lists["roles"].is_empty()),
        "{choices:?}"
    );
    assert!(check_model(&model).counterexamples.is_empty());
}

#[test]
fn undeclared_effect_inputs_also_choose_held_values() {
    let spec = ROLES.replace("params = [\"role\"]", "");
    let model = build_model_from_ioa(&spec, 2).unwrap();
    assert!(
        check_model(&model)
            .counterexamples
            .iter()
            .any(|c| c.violated.contains(&"HasRole".to_string()))
    );
}

#[test]
fn concrete_inputs_respect_the_list_membership_guard() {
    let spec = ROLES
        .replace("remove(roles, params.role)", "append(roles, params.role)")
        .replace(
            "params = [\"role\"]",
            "params = [\"role\"]\nguard = \"params.role not in roles\"",
        );
    let model = build_model_from_ioa(&spec, 2).unwrap();
    let state = model.init_states().remove(0);
    let choices = successors(&model, &model.transitions[0], &state);
    assert!(!choices.is_empty());
    assert!(
        choices.iter().all(|(params, next)| {
            params["role"] != ParamValue::Str("Cook".into()) && next.lists["roles"].len() == 2
        }),
        "{choices:?}"
    );
}
