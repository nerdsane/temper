//! Route proofs must honor the local inputs needed by each exit.

use super::{TOKEN, broken, model_check};

/// An unmodeled local integer must not act as a freely satisfiable route.
#[test]
fn an_unmodeled_integer_cannot_certify_a_route() {
    let spec = TOKEN
        .replace(
            "type = \"string\"\ninitial = \"\"",
            "type = \"int\"\ninitial = 0",
        )
        .replace("token == 'ok'", "token == 1");
    let l1 = model_check(&spec);
    assert!(!l1.passed, "{}", l1.summary);
    assert!(l1.summary.contains("not proved"), "{}", l1.summary);
    assert!(l1.summary.contains("token"), "{}", l1.summary);
    let result = l1.verification.as_ref().unwrap();
    assert!(result.is_complete, "the graph walk completed");
    assert!(
        result.counterexamples.is_empty(),
        "no concrete refusal was proved"
    );
    assert!(result.bounded_by.is_empty());
    assert_eq!(result.unproved_can_reach.len(), 1);
    assert!(result.unproved_can_reach[0].contains("can_finish"));
    // Without an author-stated route rule, numeric abstraction keeps its
    // existing behavior.
    let without_rule = spec.split("[[liveness]]").next().unwrap();
    let l1 = model_check(without_rule);
    assert!(l1.passed, "{}", l1.summary);
}

/// Constraints decide whether an exit exists even when its input does not
/// write a tracked variable and no guard or invariant reads the source.
#[test]
fn constraints_on_an_untracked_parameter_cannot_certify_an_impossible_exit() {
    let spec = TOKEN.replace(
        "guard = \"token == 'ok'\"",
        r#"
params = [{ name = "proof", type = "string" }]
[[action.constraints]]
kind = "param_nonempty"
param = "proof"
[[action.constraints]]
kind = "param_equals_field"
param = "proof"
field = "token"
"#,
    );
    let l1 = model_check(&spec);
    let (moves, place, _) = broken(&l1, "can_finish");
    assert!(moves.is_empty());
    assert_eq!(place, "Waiting");
    assert!(l1.verification.as_ref().unwrap().is_complete);
    let enabled = spec.replace("initial = \"\"", "initial = \"ok\"");
    let l1 = model_check(&enabled);
    assert!(l1.passed, "{}", l1.summary);
}

/// A parameter used for a string field and a list must keep the literals
/// required by both, including when the string value comes through a constraint.
#[test]
fn a_shared_string_and_list_parameter_keeps_list_choices() {
    let spec = format!(
        "{}{}",
        TOKEN.replace(
            "guard = \"token == 'ok'\"",
            "guard = \"!empty(token) && 'vip' in tags\""
        ),
        r#"
[[state]]
name = "tags"
type = "list"
initial = []
[[action]]
name = "Supply"
from = ["Waiting"]
to = "Waiting"
params = [{ name = "token", type = "string" }]
effect = ["append(tags, params.token)"]
"#
    );
    // Exercise the model's actual input choice and resulting enabled exit.
    let model = temper_verify::build_model_from_ioa(&spec, 2).unwrap();
    use stateright::Model;
    use temper_verify::model::semantics::{successors, truth};
    let initial = model.init_states().remove(0);
    let supply = model
        .transitions
        .iter()
        .find(|t| t.name == "Supply")
        .unwrap();
    let (_, supplied) = successors(&model, supply, &initial)
        .into_iter()
        .find(|(params, _)| params["token"] == temper_verify::ParamValue::Str("vip".into()))
        .expect("the actual vip input is explored");
    assert_eq!(
        supplied.lists["tags"],
        ["vip"].map(temper_verify::model::ModelString::from)
    );
    assert_eq!(supplied.strings["token"], "vip".into());
    let finish = model
        .transitions
        .iter()
        .find(|t| t.name == "Finish")
        .unwrap();
    assert!(truth(&finish.guard, &model.var_kinds, &supplied).must_hold());
    let result = temper_verify::check_model(&model);
    assert!(result.all_properties_hold, "{result:?}");
    assert!(result.dead_transitions.is_empty());
    let l1 = model_check(&spec);
    assert!(l1.passed, "{}", l1.summary);
    assert!(l1.verification.as_ref().unwrap().counterexamples.is_empty());

    // The same choice must reach a source field, then be copied to a list
    // by an input which does not itself write a tracked field.
    let constrained = spec.replace("params = [{ name = \"token\", type = \"string\" }]\neffect = [\"append(tags, params.token)\"]", r#"
params = [{ name = "proof", type = "string" }]
effect = ["append(tags, params.proof)"]
[[action.constraints]]
kind = "param_equals_field"
param = "proof"
field = "token"

[[action]]
name = "SupplyToken"
from = ["Waiting"]
to = "Waiting"
params = [{ name = "token", type = "string" }]
"#);
    assert_ne!(constrained, spec);
    let l1 = model_check(&constrained);
    assert!(l1.passed, "{}", l1.summary);
}

/// A guard on the chosen input cannot be treated as a free true branch.
#[test]
fn an_impossible_parameter_guard_cannot_certify_a_route() {
    for token in ["", "wrong"] {
        let spec = TOKEN
            .replace("initial = \"\"", &format!("initial = {token:?}"))
            .replace(
                "guard = \"token == 'ok'\"",
                r#"
guard = "params.proof == 'ok'"
params = [{ name = "proof", type = "string" }]
[[action.constraints]]
kind = "param_equals_field"
param = "proof"
field = "token"
"#,
            );
        let l1 = model_check(&spec);
        assert!(!l1.passed, "{}", l1.summary);
        let result = l1.verification.as_ref().unwrap();
        assert!(result.is_complete);
        assert!(result.bounded_by.is_empty());
        assert!(
            result
                .counterexamples
                .iter()
                .any(|c| c.property == "CanReach")
                || result
                    .unproved_can_reach
                    .iter()
                    .any(|r| r.contains("can_finish") && r.contains("params.proof")),
            "{result:?}"
        );
    }
}

/// No named literal is needed to preserve one input in two kinds of storage.
#[test]
fn a_shared_string_and_list_input_keeps_its_identity() {
    let spec = r#"
[automaton]
name = "SharedInput"
states = ["Waiting", "Ready", "Done"]
initial = "Waiting"
[[state]]
name = "token"
type = "string"
initial = ""
[[state]]
name = "tags"
type = "list"
initial = []
[[action]]
name = "Supply"
from = ["Waiting"]
to = "Ready"
params = [{ name = "token", type = "string" }]
effect = ["append(tags, params.token)"]
[[action]]
name = "Finish"
from = ["Ready"]
to = "Done"
guard = "!empty(token) && token in tags"
[[liveness]]
name = "can_finish"
from = ["Waiting"]
can_reach = ["Done"]
"#;
    let l1 = model_check(spec);
    assert!(l1.passed, "{}", l1.summary);
    let model = temper_verify::build_model_from_ioa(spec, 2).unwrap();
    use stateright::Model;
    use temper_verify::model::semantics::{successors, truth};
    let initial = model.init_states().remove(0);
    let supply = model
        .transitions
        .iter()
        .find(|t| t.name == "Supply")
        .unwrap();
    let finish = model
        .transitions
        .iter()
        .find(|t| t.name == "Finish")
        .unwrap();
    for (_, next) in successors(&model, supply, &initial) {
        assert_eq!(
            truth(&finish.guard, &model.var_kinds, &next).must_hold(),
            !next.strings["token"].is_empty()
        );
    }
    // A list-only input remains available to a later string input even
    // when no field held it in the intervening state.
    let list_first = spec
        .replace(
            r#"states = ["Waiting", "Ready", "Done"]"#,
            r#"states = ["Waiting", "Supplied", "Ready", "Done"]"#,
        )
        .replace(
            r#"to = "Ready"
params = [{ name = "token", type = "string" }]
effect = ["append(tags, params.token)"]"#,
            r#"to = "Supplied"
params = [{ name = "tag", type = "string" }]
effect = ["append(tags, params.tag)"]
[[action]]
name = "SupplyToken"
from = ["Supplied"]
to = "Ready"
params = [{ name = "token", type = "string" }]"#,
        );
    assert_ne!(list_first, spec);
    let l1 = model_check(&list_first);
    assert!(l1.passed, "{}", l1.summary);

    // A real mismatch must still refuse.
    let mismatch = spec.replace("append(tags, params.token)", "append(tags, 'other')");
    let l1 = model_check(&mismatch.replace(
        "from = [\"Waiting\"]\ncan_reach",
        "from = [\"Ready\"]\ncan_reach",
    ));
    broken(&l1, "can_finish");
}

/// Compared variables must share literals even before either has been supplied.
#[test]
fn compared_strings_share_route_literals() {
    let spec = r#"
[automaton]
name = "ComparedInputs"
states = ["Start", "Middle", "Waiting", "Done"]
initial = "Start"
[[state]]
name = "a"
type = "string"
initial = ""
[[state]]
name = "b"
type = "string"
initial = ""
[[action]]
name = "SupplyA"
from = ["Start"]
to = "Middle"
params = [{ name = "a", type = "string" }]
[[action]]
name = "SupplyB"
from = ["Middle"]
to = "Waiting"
params = [{ name = "b", type = "string" }]
[[action]]
name = "Finish"
from = ["Waiting"]
to = "Done"
guard = "a == b && b == 'ok'"
[[liveness]]
name = "can_finish"
from = ["Start"]
can_reach = ["Done"]
"#;
    let l1 = model_check(spec);
    assert!(l1.passed, "{}", l1.summary);
}
