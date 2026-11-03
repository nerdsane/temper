//! Route checks retain guard-only values when the same model checks entries.

use super::{TOKEN, broken, model_check};

#[test]
fn an_entry_rule_does_not_hide_a_route_closed_by_a_string_input() {
    let spec = format!(
        "{}{}",
        TOKEN
            .replace("initial = \"\"", "initial = \"ok\"")
            .replace(
                "states = [\"Waiting\", \"Done\"]",
                "states = [\"Waiting\", \"Replaced\", \"Done\"]"
            )
            .replace("from = [\"Waiting\"]", "from = [\"Waiting\", \"Replaced\"]"),
        r#"
[[state]]
name = "replaced"
type = "bool"
initial = false

[[action]]
name = "ReplaceToken"
from = ["Waiting"]
to = "Replaced"
params = [{ name = "token", type = "string" }]
guard = "!replaced"
effect = ["replaced = true"]

[[entry_rule]]
name = "only_an_unused_replacement"
place = "Replaced"
assert = "!replaced"
"#
    );
    let l1 = model_check(&spec);
    let (moves, place, _) = broken(&l1, "can_finish");
    assert_eq!(moves, ["ReplaceToken"]);
    assert_eq!(place, "Replaced");
    let result = l1.verification.as_ref().unwrap();
    assert!(result.is_complete);
    assert!(result.bounded_by.is_empty());
    assert!(result.unproved_can_reach.is_empty());
    let [refusal] = result.counterexamples.as_slice() else {
        panic!("expected only the broken route: {result:?}");
    };
    assert_eq!(refusal.never_reaches, ["Done"]);
    let (last, _) = refusal.trace.last().unwrap();
    assert!(last.booleans["replaced"]);
    assert_ne!(last.strings["token"], "ok".into());

    // Restoring the token keeps the route open while still satisfying
    // the entry rule on every move into Replaced.
    let recoverable = format!(
        "{spec}{}",
        r#"
[[action]]
name = "RestoreToken"
from = ["Replaced"]
to = "Waiting"
effect = ["token = 'ok'", "replaced = false"]
"#
    );
    let l1 = model_check(&recoverable);
    assert!(l1.passed, "{}", l1.summary);
}

/// Entry checking evaluates each selected input against the combined guard.
/// Its route proof must account for that enforcement for every modeled kind.
#[test]
fn checked_inputs_can_certify_a_route_when_entry_rules_exist() {
    for (kind, initial, value, other) in [
        ("string", "\"ok\"", "'ok'", "'wrong'"),
        ("counter", "1", "1", "0"),
        ("bool", "true", "true", "false"),
    ] {
        for guarded in [false, true] {
            let guard = if guarded {
                format!("guard = \"params.proof == {value}\"\n")
            } else {
                String::new()
            };
            let spec = TOKEN
                .replace(
                    "type = \"string\"\ninitial = \"\"",
                    &format!("type = {kind:?}\ninitial = {initial}"),
                )
                .replace(
                    "guard = \"token == 'ok'\"",
                    &format!(
                        r#"
{guard}params = [{{ name = "proof", type = {kind:?} }}]
[[action.constraints]]
kind = "param_equals_field"
param = "proof"
field = "token"
"#
                    ),
                );
            // Without entry rules, numeric/boolean constraints and parameter
            // guards are still unchecked. String equality alone is enforced.
            let l1 = model_check(&spec);
            assert_eq!(l1.passed, kind == "string" && !guarded, "{}", l1.summary);
            let spec = format!(
                r#"{spec}
[[entry_rule]]
name = "valid_token"
place = "Done"
assert = "token == {value}"
"#
            );
            let l1 = model_check(&spec);
            assert!(l1.passed, "{kind}, guarded={guarded}: {}", l1.summary);
            let result = l1.verification.as_ref().unwrap();
            assert!(result.is_complete);
            assert!(result.bounded_by.is_empty());
            assert!(result.counterexamples.is_empty());
            assert!(result.unproved_can_reach.is_empty());
            // The same enforced comparison must refuse an impossible exit.
            let impossible = spec.replace(
                "kind = \"param_equals_field\"",
                r#"kind = "param_not_equals_field"
param = "proof"
field = "token"
[[action.constraints]]
kind = "param_equals_field""#,
            );
            broken(&model_check(&impossible), "can_finish");
            if guarded {
                // Guard-only inputs are chosen too, even without a constraint.
                let unconstrained = spec.replace(
                    "[[action.constraints]]\nkind = \"param_equals_field\"\nparam = \"proof\"\nfield = \"token\"",
                    "",
                );
                let l1 = model_check(&unconstrained);
                assert!(l1.passed, "{}", l1.summary);
                let impossible = spec.replace(
                    &format!("params.proof == {value}"),
                    &format!("params.proof == {other}"),
                );
                broken(&model_check(&impossible), "can_finish");
            } else {
                for comparison in ["param_not_equals_field", "param_greater_than_field"] {
                    if comparison == "param_greater_than_field" && kind != "counter" {
                        continue;
                    }
                    let alternate = spec.replace("param_equals_field", comparison);
                    let l1 = model_check(&alternate);
                    assert!(l1.passed, "{kind}, {comparison}: {}", l1.summary);
                }
            }
        }
    }
}

/// Entry mode still leaves unsupported numeric fields and inputs unknown.
#[test]
fn entry_rules_do_not_certify_unmodeled_inputs() {
    let spec = format!(
        r#"{}
[[entry_rule]]
name = "valid_entry"
place = "Done"
assert = "true"
"#,
        TOKEN
            .replace(
                "type = \"string\"\ninitial = \"\"",
                "type = \"int\"\ninitial = 0"
            )
            .replace(
                "guard = \"token == 'ok'\"",
                r#"
params = [{ name = "proof", type = "int" }]
[[action.constraints]]
kind = "param_equals_field"
param = "proof"
field = "token"
"#
            )
    );
    let l1 = model_check(&spec);
    assert!(!l1.passed, "{}", l1.summary);
    let result = l1.verification.as_ref().unwrap();
    assert!(result.is_complete);
    assert!(result.counterexamples.is_empty(), "{result:?}");
    assert_eq!(result.unproved_can_reach.len(), 1);
    assert!(result.unproved_can_reach[0].contains("token"));
    assert!(result.unproved_can_reach[0].contains("proof"));
}

#[test]
fn an_entry_rule_does_not_make_an_unknown_parameter_checked() {
    let spec = format!(
        r#"{}
[[entry_rule]]
name = "valid_entry"
place = "Done"
assert = "true"
"#,
        TOKEN.replace(
            "guard = \"token == 'ok'\"",
            r#"guard = "params.proof > 0"
params = [{ name = "proof", type = "int" }]"#,
        )
    );
    let l1 = model_check(&spec);
    assert!(!l1.passed, "{}", l1.summary);
    let result = l1.verification.as_ref().unwrap();
    assert!(result.is_complete);
    assert!(result.counterexamples.is_empty(), "{result:?}");
    assert_eq!(result.unproved_can_reach.len(), 1);
    assert!(result.unproved_can_reach[0].contains("params.proof"));
}
