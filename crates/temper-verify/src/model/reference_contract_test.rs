use stateright::Model;

use super::build_model_from_ioa;
use super::reference_contract::finite_assignments;

const SPEC: &str = r#"
[automaton]
name = "Document"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "workspace_id"
type = "ref"
entity_type = "Workspace"
initial = ""

[[action]]
name = "Attach"
kind = "input"
from = ["Active"]
to = "Active"
params = [{ name = "workspace_id", type = "ref", entity_type = "Workspace" }]

[[action]]
name = "Confirm"
kind = "input"
from = ["Active"]
to = "Active"
params = [{ name = "candidate", type = "ref", entity_type = "Workspace" }]
guard = [{ type = "reference_equals", reference = "workspace_id", param = "candidate" }]
"#;

#[test]
fn finite_inputs_include_the_rejected_unset_class() {
    let assignments = finite_assignments(&[("workspace_id".into(), 2)]);
    assert_eq!(assignments.len(), 3);
    assert_eq!(assignments[0]["workspace_id"], 0);
}

#[test]
fn finite_identity_classes_distinguish_equal_and_unequal_references() {
    let model = build_model_from_ioa(SPEC, 3).unwrap();
    let initial = model.init_states().remove(0);
    let mut actions = Vec::new();
    model.actions(&initial, &mut actions);
    let attach = actions
        .into_iter()
        .find(|action| action.name == "Attach" && action.reference_params["workspace_id"] == 1)
        .unwrap();
    let attached = model.next_state(&initial, attach).unwrap();

    let mut actions = Vec::new();
    model.actions(&attached, &mut actions);
    let confirms = actions
        .iter()
        .filter(|action| action.name == "Confirm")
        .collect::<Vec<_>>();
    assert_eq!(confirms.len(), 1);
    assert_eq!(confirms[0].reference_params["candidate"], 1);
    assert!(!attached.counters.contains_key("__ref:candidate"));
}

#[test]
fn finite_identity_budget_keeps_target_type_namespaces_disjoint() {
    let spec = r#"
[automaton]
name = "Membership"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "workspace_id"
type = "ref"
entity_type = "Workspace"
initial = ""

[[state]]
name = "user_id"
type = "ref"
entity_type = "User"
initial = ""

[[action]]
name = "Attach"
kind = "input"
from = ["Active"]
to = "Active"
params = [
  { name = "workspace_id", type = "ref", entity_type = "Workspace" },
  { name = "user_id", type = "ref", entity_type = "User" },
]
"#;
    let model = build_model_from_ioa(spec, 3).unwrap();
    let initial = model.init_states().remove(0);
    assert_eq!(initial.counters["__ref:workspace_id"], 0);
    assert_eq!(initial.counters["__ref:user_id"], 0);

    let mut actions = Vec::new();
    model.actions(&initial, &mut actions);
    let attach = actions
        .iter()
        .filter(|action| action.name == "Attach")
        .collect::<Vec<_>>();
    // Each target type has its own R + P + E = 1 + 1 + 0 namespace.
    assert_eq!(attach.len(), 4);
    assert!(attach.iter().all(|action| {
        (1..=2).contains(&action.reference_params["workspace_id"])
            && (1..=2).contains(&action.reference_params["user_id"])
    }));

    let distinct = attach
        .into_iter()
        .find(|action| {
            action.reference_params["workspace_id"] == 2 && action.reference_params["user_id"] == 2
        })
        .unwrap()
        .clone();
    let attached = model.next_state(&initial, distinct).unwrap();
    assert_eq!(attached.counters["__ref:workspace_id"], 1);
    assert_eq!(attached.counters["__ref:user_id"], 1);
}

#[test]
fn deterministic_key_initial_states_cover_same_and_distinct_identity_tuples() {
    let spec = r#"
[automaton]
name = "Edge"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "left_id"
type = "ref"
entity_type = "Node"
initial = ""

[[state]]
name = "right_id"
type = "ref"
entity_type = "Node"
initial = ""

[[key]]
name = "edge"
properties = ["left_id", "right_id"]
entity_id = true
"#;
    let model = build_model_from_ioa(spec, 3).unwrap();
    let states = model.init_states();
    assert_eq!(states.len(), 2);
    assert!(states.iter().any(|state| {
        state.counters["__ref:left_id"] == 1 && state.counters["__ref:right_id"] == 1
    }));
    assert!(states.iter().any(|state| {
        state.counters["__ref:left_id"] == 1 && state.counters["__ref:right_id"] == 2
    }));
    for state in states {
        assert_eq!(state.counters["__id:0"], state.counters["__ref:left_id"]);
        assert_eq!(state.counters["__id:1"], state.counters["__ref:right_id"]);
    }
}

#[test]
fn atomic_creates_receive_declaration_ordered_fresh_symbols() {
    let spec = r#"
[automaton]
name = "Batch"
states = ["Active"]
initial = "Active"
allow_indefinite_states = ["Active"]

[[state]]
name = "workspace_id"
type = "ref"
entity_type = "Workspace"
initial = ""

[[action]]
name = "CreateChildren"
kind = "Composite"
from = ["Active"]
to = "Active"
params = [{ name = "candidate", type = "ref", entity_type = "Child" }]

[[action.sub_writes]]
target_entity = "Child"
action = "Create"

[[action.sub_writes]]
target_entity = "Child"
action = "Create"
"#;
    let model = build_model_from_ioa(spec, 3).unwrap();
    let initial = model.init_states().remove(0);
    let mut actions = Vec::new();
    model.actions(&initial, &mut actions);
    let action = actions
        .iter()
        .find(|action| action.name == "CreateChildren")
        .unwrap();
    // Child has R=0, P=1, E=2, so the two creates own classes 2 and 3.
    assert_eq!(action.fresh_references["Child"], vec![2, 3]);
}
