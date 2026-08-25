use super::parse_toml_to_automaton;

#[test]
fn canonicalized_nested_field_invariant_and_timeout_params_round_trip() {
    let source = r#"
[automaton]
name = "Session"
states = ["Created", "Failed"]
initial = "Created"

[[state]]
name = "lifecycle"
type = "status"
initial = "Created"

[[action]]
name = "Fail"
kind = "input"
from = ["Created"]
to = "Failed"
params = ["error_message"]

[[field_invariant]]
name = "FailedRequiresError"
when = { field = "lifecycle", equals = "Failed" }
require = { not = { field = "error_message", empty = true } }
message = "failed sessions retain their error"

[[state_timeout]]
state = "Created"
after_seconds = 60
on_timeout = "Fail"
params = { error_message = "configuration never arrived" }
"#;
    let value = toml::from_str::<toml::Value>(source).unwrap();
    let canonical = toml::to_string(&value).unwrap();

    let parsed = parse_toml_to_automaton(&canonical)
        .expect("canonical nested metadata should remain parseable");
    assert_eq!(parsed.field_invariants.len(), 1);
    assert_eq!(
        parsed.state_timeouts[0]
            .params
            .get("error_message")
            .map(String::as_str),
        Some("configuration never arrived")
    );
}
