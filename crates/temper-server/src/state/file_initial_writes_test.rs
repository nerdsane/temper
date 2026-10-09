use super::*;

#[test]
fn round_four_atomic_file_initial_state_materializes_declared_defaults() {
    let table = temper_jit::table::TransitionTable::from_ioa_source(
        r#"
[automaton]
name = "File"
states = ["Active"]
initial = "Active"
strict_action_params = true
[[state]]
name = "revision"
type = "counter"
initial = 3
[[action]]
name = "StreamUpdated"
kind = "input"
from = ["Active"]
to = "Active"
params = ["expected"]
constraints = [{kind="param_equals_field",param="expected",field="revision"}]
"#,
    );
    let mut state = initial_file_state("file", &table, serde_json::json!({}));
    assert_eq!(state.counters.get("revision"), Some(&3));
    state.fields["Title"] = serde_json::json!("legitimate initial field");
    let created: EntityEvent = serde_json::from_value(serde_json::json!({
        "action":"Created", "from_status":"", "to_status":"Active",
        "timestamp":sim_now(), "params":{}
    }))
    .unwrap();
    let envelope = synthetic_envelope("default:File:file", 1, &created, &state, &table).unwrap();
    assert_eq!(envelope.payload["params"], created.params);
    assert_eq!(
        envelope.payload["initial_values"]["fields"],
        crate::entity_actor::effects::sanitize_action_params(&state.fields).into_owned()
    );
    assert_eq!(
        envelope.payload["initial_values"]["counters"]["revision"],
        3
    );

    let result = apply_synthetic_file_action(
        &mut state,
        &table,
        "StreamUpdated",
        serde_json::json!({"expected":3}),
        &Default::default(),
    );
    assert!(
        result.is_ok(),
        "fresh File cannot use its declared initial state: {result:?}"
    );
    let action = result.unwrap();
    let envelope = synthetic_envelope("default:File:file", 2, &action, &state, &table).unwrap();
    assert_eq!(
        envelope.payload,
        serde_json::to_value(action).unwrap(),
        "non-bootstrap action parameters must remain byte-identical"
    );
}
