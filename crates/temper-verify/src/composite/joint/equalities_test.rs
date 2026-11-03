//! A no-write parameter must satisfy every equality before a reaction lands.

use super::*;

/// The liveness results for `refs`, with `analysis` deciding each move as its
/// sender, in the same room as `check_under`.
fn liveness(refs: &[&Automaton], analysis: &SendAnalysis) -> Vec<JointLivenessResult> {
    let (room, progress) = (Bytes::new(1 << 30), Progress::default());
    let bound = Bound {
        room: &room,
        progress: &progress,
    };
    verify_joint_liveness(refs, Some(analysis), bound)
}

const PARENT: &str = r#"
[automaton]
name = "Parent"
states = ["Open", "Closed"]
initial = "Open"

[[state]]
name = "value"
type = "string"
initial = "b"

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"

[[action.triggers]]
name = "seal"
kind = "entity"
principal = "tell"
target_entity = "Child"
target_action = "Seal"

[action.triggers.args]
sealed_with = "value"

[action.triggers.resolve_target]
kind = "same_id"

[[liveness]]
name = "seals"
eventually = "status == 'Closed' && Child[Id].status == 'Done' => Child[Id].status == 'Sealed'"
relies_on = ["Close"]
"#;

const CHILD: &str = r#"
[automaton]
name = "Child"
states = ["Done", "Sealed"]
initial = "Done"

[[state]]
name = "a"
type = "string"
initial = ""

[[state]]
name = "b"
type = "string"
initial = "b"

[[action]]
name = "Seal"
kind = "internal"
from = ["Done"]
to = "Sealed"
params = [{ name = "sealed_with", type = "string" }]

[[action.constraints]]
kind = "param_equals_field"
param = "sealed_with"
field = "a"

[[action.constraints]]
kind = "param_equals_field"
param = "sealed_with"
field = "b"

[[invariant]]
name = "seal_requires_equal_fields"
assert = "status == 'Sealed' => a == b && Parent[Id].status == 'Closed'"
"#;

#[test]
fn a_reaction_cannot_satisfy_conflicting_equalities_with_the_last_field() {
    // `a` and `b` never agree, so the rule breaks if a seal ever lands.
    let result = check_under(&[PARENT, CHILD], Some(ONLY_TELL_REACHES_THE_CHILD));
    assert!(never_meets(&result, "status == 'Sealed'"), "{result}");
}

#[test]
fn liveness_checks_every_equality_before_counting_on_a_reaction() {
    for (child, should_settle) in [
        (CHILD.to_string(), false),
        (CHILD.replace("initial = \"\"", "initial = \"b\""), true),
    ] {
        let automatons = parse(&[PARENT, &child]);
        let refs: Vec<&Automaton> = automatons.iter().collect();
        let analysis = SendAnalysis::new(ONLY_TELL_REACHES_THE_CHILD).unwrap();
        let mut results = liveness(&refs, &analysis);
        assert_eq!(results.len(), 1);
        let result = results.remove(0);
        if should_settle {
            assert!(result.holds(), "matching equalities: {result}");
        } else {
            assert!(
                matches!(result.outcome, LivenessOutcome::Stuck { .. }),
                "{result}"
            );
        }
    }
}

/// The reaction sends the same value held by both equality fields, so only
/// the nonempty constraint can refuse it. Neither entity writes sealed_with.
fn nonempty_pair(value: &str) -> (String, String) {
    let initial = format!("initial = {}", serde_json::to_string(value).unwrap());
    let parent = PARENT.replace("initial = \"b\"", &initial);
    let child = CHILD
        .replace("initial = \"\"", &initial)
        .replace("initial = \"b\"", &initial)
        .replace(
            "[[invariant]]",
            r#"[[action.constraints]]
kind = "param_nonempty"
param = "sealed_with"

[[invariant]]"#,
        );
    (parent, child)
}

#[test]
fn a_reaction_with_a_whitespace_only_parameter_is_refused() {
    for value in ["", " ", "\t\n", "\u{2003}"] {
        let (parent, child) = nonempty_pair(value);
        let child = child.replace(
            "status == 'Sealed' => a == b && Parent[Id].status == 'Closed'",
            "status == 'Sealed' => a == b && Parent[Id].status == 'Open'",
        );
        // A seal comes only once the parent is Closed, so the rule breaks if
        // one ever lands.
        let result = check_under(&[&parent, &child], Some(ONLY_TELL_REACHES_THE_CHILD));
        assert!(
            never_meets(&result, "status == 'Sealed'"),
            "{value:?}: {result}"
        );
    }
}

#[test]
fn a_whitespace_only_parameter_cannot_prove_a_reaction_settles() {
    for (value, should_settle) in [
        ("", false),
        (" ", false),
        ("\t\n", false),
        ("\u{2003}", false),
        (" b ", true),
        ("b", true),
    ] {
        let (parent, child) = nonempty_pair(value);
        let automatons = parse(&[&parent, &child]);
        let refs: Vec<&Automaton> = automatons.iter().collect();
        let analysis = SendAnalysis::new(ONLY_TELL_REACHES_THE_CHILD).unwrap();
        let mut results = liveness(&refs, &analysis);
        assert_eq!(results.len(), 1);
        let result = results.remove(0);
        if should_settle {
            assert!(result.holds(), "{value:?}: {result}");
        } else {
            assert!(
                matches!(result.outcome, LivenessOutcome::Stuck { .. }),
                "{value:?}: {result}"
            );
        }
    }
}

/// A value copied once, then used to seal after the copy has arrived.
const COPYING_PARENT: &str = r#"
[automaton]
name = "Parent"
states = ["New", "Open", "Closed"]
initial = "New"

[[state]]
name = "value"
type = "string"
initial = ""

[[action]]
name = "Set"
kind = "input"
from = ["New"]
to = "Open"
params = [{ name = "value", type = "string" }]

[[action.triggers]]
name = "copy"
kind = "entity"
principal = "tell"
target_entity = "Child"
target_action = "Copy"

[action.triggers.args]
value = "value"

[action.triggers.resolve_target]
kind = "same_id"

[[action]]
name = "Close"
kind = "input"
from = ["Open"]
to = "Closed"
guard = "Child[Id].status == 'Done' && value != ''"

[[action.triggers]]
name = "seal"
kind = "entity"
principal = "tell"
target_entity = "Child"
target_action = "Seal"

[action.triggers.args]
sealed_with = "value"

[action.triggers.resolve_target]
kind = "same_id"

[[liveness]]
name = "seals"
eventually = "status == 'Closed' => (Child[Id].status == 'Sealed' && Child[Id].value == value)"
relies_on = ["Close"]
"#;
const COPYING_CHILD: &str = r#"
[automaton]
name = "Child"
states = ["Waiting", "Done", "Sealed"]
initial = "Waiting"

[[state]]
name = "value"
type = "string"
initial = ""

[[action]]
name = "Copy"
kind = "internal"
from = ["Waiting"]
to = "Done"
params = [{ name = "value", type = "string" }]

[[action]]
name = "Seal"
kind = "internal"
from = ["Done"]
to = "Sealed"
params = [{ name = "sealed_with", type = "string" }]

[[action.constraints]]
kind = "param_equals_field"
param = "sealed_with"
field = "value"

[[action.constraints]]
kind = "param_nonempty"
param = "sealed_with"
"#;
/// Whitespace arrives from an unconstrained input, then survives a copy.
/// There is deliberately no whitespace literal anywhere in either spec.
#[test]
fn an_unconstrained_input_can_copy_whitespace_that_prevents_sealing() {
    let policies = r#"
permit(principal, action in [Action::"Set", Action::"Close"], resource);
permit(principal == Agent::"service:tell", action in [Action::"Copy", Action::"Seal"], resource is Child);
"#;
    // Constraining the original input to nonblank is a positive control:
    // its copy must still settle, including when it uses a fresh symbol.
    for nonblank_input in [false, true] {
        let parent = if nonblank_input {
            COPYING_PARENT.replace(
                "[[action.triggers]]\nname = \"copy\"",
                "[[action.constraints]]\nkind = \"param_nonempty\"\nparam = \"value\"\n\n[[action.triggers]]\nname = \"copy\"",
            )
        } else {
            COPYING_PARENT.to_string()
        };
        let automatons = parse(&[&parent, COPYING_CHILD]);
        let refs: Vec<&Automaton> = automatons.iter().collect();
        let analysis = SendAnalysis::new(policies).unwrap();
        let mut results = liveness(&refs, &analysis);
        assert_eq!(results.len(), 1);
        let result = results.remove(0);
        if nonblank_input {
            assert!(result.holds(), "nonblank input: {result}");
        } else {
            assert!(
                matches!(result.outcome, LivenessOutcome::Stuck { .. }),
                "whitespace input must be able to get stuck: {result}"
            );
        }
    }
}

/// An ID is another unconstrained string source: whitespace must survive
/// copying from it even when neither spec contains a whitespace literal.
#[test]
fn a_whitespace_only_id_can_copy_a_value_that_prevents_sealing() {
    let parent = COPYING_PARENT
        .replace("params = [{ name = \"value\", type = \"string\" }]", "")
        .replace("value = \"value\"", "value = \"Id\"")
        .replace("sealed_with = \"value\"", "sealed_with = \"Id\"")
        .replace(" && value != ''", "")
        .replace(
            "(Child[Id].status == 'Sealed' && Child[Id].value == value)",
            "(Child[Id].status == 'Sealed' && Child[Id].value != '')",
        );
    let analysis = SendAnalysis::new(ONLY_TELL_REACHES_THE_CHILD).unwrap();
    for nonempty in [false, true] {
        let child = if nonempty {
            COPYING_CHILD.to_string()
        } else {
            COPYING_CHILD.replace(
                "[[action.constraints]]\nkind = \"param_nonempty\"\nparam = \"sealed_with\"\n",
                "",
            )
        };
        let automatons = parse(&[&parent, &child]);
        let refs: Vec<&Automaton> = automatons.iter().collect();
        let results = liveness(&refs, &analysis);
        assert_eq!(results.len(), 1);
        let result = &results[0];
        if nonempty {
            assert!(
                matches!(result.outcome, LivenessOutcome::Stuck { .. }),
                "whitespace ID must be able to get stuck: {result}"
            );
        } else {
            assert!(result.holds(), "without nonempty, all IDs seal: {result}");
        }
    }
}
