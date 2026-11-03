//! Merge regressions: string effects and route input identities share one model.

use temper_verify::checker::check_model;
use temper_verify::model::build_model_from_ioa;

#[test]
fn an_input_can_match_a_later_string_effect_and_its_own_list_entry() {
    let source = r#"
[automaton]
name = "FutureEffectRoute"
states = ["Start", "Supplied", "Written", "Done"]
initial = "Start"
terminal = ["Done"]
[[state]]
name = "input"
type = "string"
initial = ""
[[state]]
name = "literal"
type = "string"
initial = ""
[[state]]
name = "tags"
type = "list"
initial = []
[[action]]
name = "Supply"
from = ["Start"]
to = "Supplied"
params = ["input"]
effect = ["append(tags, params.input)"]
[[action]]
name = "Write"
from = ["Supplied"]
to = "Written"
effect = ["literal = 'future'"]
[[action]]
name = "Finish"
from = ["Written"]
to = "Done"
guard = "input == literal && input in tags"
[[liveness]]
name = "can_finish"
from = ["Start"]
can_reach = ["Done"]
"#;
    let result = check_model(&build_model_from_ioa(source, 2).unwrap());
    assert!(
        result.is_complete && result.all_properties_hold,
        "{result:#?}"
    );
    assert!(result.unproved_can_reach.is_empty(), "{result:#?}");
    assert!(result.bounded_by.is_empty(), "{result:#?}");
}

#[test]
fn a_string_effect_that_closes_the_route_still_refuses() {
    let source = r#"
[automaton]
name = "ClearedRoute"
states = ["Waiting", "Done"]
initial = "Waiting"
terminal = ["Done"]
[[state]]
name = "token"
type = "string"
initial = "ok"
[[action]]
name = "Finish"
from = ["Waiting"]
to = "Done"
guard = "token == 'ok'"
[[action]]
name = "Clear"
from = ["Waiting"]
to = "Waiting"
effect = ["token = ''"]
[[liveness]]
name = "can_finish"
from = ["Waiting"]
can_reach = ["Done"]
"#;
    let result = check_model(&build_model_from_ioa(source, 2).unwrap());
    assert!(
        result.is_complete && !result.all_properties_hold,
        "{result:#?}"
    );
    let example = result
        .counterexamples
        .iter()
        .find(|example| example.property == "CanReach")
        .expect("clearing the token strands the record");
    assert_eq!(example.violated, ["can_finish"]);
    assert_eq!(example.never_reaches, ["Done"]);
    assert_eq!(example.trace.len(), 2);
    assert_eq!(example.trace[0].1.as_ref().unwrap().name, "Clear");
    assert!(example.trace[1].0.strings["token"].is_empty());

    let control = source.replace("token = ''", "token = 'ok'");
    let result = check_model(&build_model_from_ioa(&control, 2).unwrap());
    assert!(
        result.is_complete && result.all_properties_hold,
        "{result:#?}"
    );
}
