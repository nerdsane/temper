use super::{StateVarInitialValue, TransitionTable};

#[test]
fn state_initials_use_the_shared_model_and_runtime_parsers() {
    let spec = r#"
[automaton]
name = "TypedInitials"
states = ["Ready"]
initial = "Ready"
allow_indefinite_states = ["Ready"]

[[state]]
name = "enabled"
type = "bool"
initial = "YES"

[[state]]
name = "retries"
type = "counter"
initial = " 3 "
"#;

    let table = TransitionTable::from_ioa_source(spec);
    assert_eq!(
        table.state_var_initials.get("enabled"),
        Some(&StateVarInitialValue::Bool(true))
    );
    assert_eq!(
        table.state_var_initials.get("retries"),
        Some(&StateVarInitialValue::Counter(3))
    );
}
