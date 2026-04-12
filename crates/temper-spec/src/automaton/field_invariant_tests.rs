use super::*;
use serde::Deserialize;
use serde_json::json;

fn predicate(toml_src: &str) -> FieldPredicate {
    #[derive(Deserialize)]
    struct W {
        p: FieldPredicate,
    }
    let wrapped = format!("p = {toml_src}");
    toml::from_str::<W>(&wrapped)
        .unwrap_or_else(|e| panic!("failed to parse predicate {toml_src:?}: {e}"))
        .p
}

fn predicate_err(toml_src: &str) -> String {
    #[derive(Deserialize)]
    struct W {
        #[serde(rename = "p")]
        _p: FieldPredicate,
    }
    let wrapped = format!("p = {toml_src}");
    toml::from_str::<W>(&wrapped)
        .err()
        .map(|e| e.to_string())
        .unwrap_or_else(|| panic!("expected parse error for {toml_src:?}"))
}

#[test]
fn absent_leaf_matches_missing_or_null() {
    let p = predicate(r#"{ field = "X", absent = true }"#);
    assert!(p.evaluate(&json!({})));
    assert!(p.evaluate(&json!({ "X": null })));
    assert!(!p.evaluate(&json!({ "X": "v" })));
    assert!(!p.evaluate(&json!({ "X": false })));
}

#[test]
fn absent_false_is_rejected() {
    let msg = predicate_err(r#"{ field = "X", absent = false }"#);
    assert!(msg.contains("absent = false"), "error was: {msg}");
}

#[test]
fn equals_leaf_matches_string() {
    let p = predicate(r#"{ field = "ConfigType", equals = "Local" }"#);
    assert!(p.evaluate(&json!({ "ConfigType": "Local" })));
    assert!(!p.evaluate(&json!({ "ConfigType": "Cloud" })));
    assert!(!p.evaluate(&json!({})));
    assert!(!p.evaluate(&json!({ "ConfigType": null })));
}

#[test]
fn equals_leaf_matches_bool() {
    let p = predicate(r#"{ field = "Allowed", equals = true }"#);
    assert!(p.evaluate(&json!({ "Allowed": true })));
    assert!(!p.evaluate(&json!({ "Allowed": false })));
    assert!(!p.evaluate(&json!({ "Allowed": "true" })));
}

#[test]
fn equals_leaf_matches_integer() {
    let p = predicate(r#"{ field = "Count", equals = 3 }"#);
    assert!(p.evaluate(&json!({ "Count": 3 })));
    assert!(!p.evaluate(&json!({ "Count": 4 })));
}

#[test]
fn empty_leaf_matches_missing_null_empty_string_empty_array() {
    let p = predicate(r#"{ field = "Hosts", empty = true }"#);
    assert!(p.evaluate(&json!({})));
    assert!(p.evaluate(&json!({ "Hosts": null })));
    assert!(p.evaluate(&json!({ "Hosts": "" })));
    assert!(p.evaluate(&json!({ "Hosts": [] })));
    assert!(!p.evaluate(&json!({ "Hosts": ["a"] })));
    assert!(!p.evaluate(&json!({ "Hosts": "a" })));
}

#[test]
fn any_of_short_circuits_on_first_pass() {
    let p = predicate(
        r#"{ any_of = [
          { field = "A", absent = true },
          { field = "B", equals = false },
        ]}"#,
    );
    assert!(p.evaluate(&json!({ "B": false })));
    assert!(p.evaluate(&json!({})));
    assert!(!p.evaluate(&json!({ "A": 1, "B": true })));
}

#[test]
fn all_of_short_circuits_on_first_fail() {
    let p = predicate(
        r#"{ all_of = [
          { field = "A", equals = "x" },
          { field = "B", equals = "y" },
        ]}"#,
    );
    assert!(p.evaluate(&json!({ "A": "x", "B": "y" })));
    assert!(!p.evaluate(&json!({ "A": "x", "B": "z" })));
    assert!(!p.evaluate(&json!({ "A": "q", "B": "y" })));
}

#[test]
fn not_inverts_child() {
    let p = predicate(r#"{ not = { field = "A", equals = "x" } }"#);
    assert!(p.evaluate(&json!({ "A": "y" })));
    assert!(!p.evaluate(&json!({ "A": "x" })));
    assert!(p.evaluate(&json!({})));
}

#[test]
fn nested_combinators_work() {
    let p = predicate(
        r#"{ all_of = [
          { field = "Type", equals = "Local" },
          { any_of = [
            { field = "Flag", absent = true },
            { field = "Flag", equals = false },
          ]},
        ]}"#,
    );
    assert!(p.evaluate(&json!({ "Type": "Local" })));
    assert!(p.evaluate(&json!({ "Type": "Local", "Flag": false })));
    assert!(!p.evaluate(&json!({ "Type": "Local", "Flag": true })));
    assert!(!p.evaluate(&json!({ "Type": "Cloud" })));
}

#[test]
fn referenced_fields_collects_all_leaves() {
    let p = predicate(
        r#"{ all_of = [
          { field = "A", equals = "x" },
          { any_of = [
            { field = "B", absent = true },
            { not = { field = "C", empty = true } },
          ]},
        ]}"#,
    );
    let refs = p.referenced_fields();
    assert_eq!(
        refs.into_iter().collect::<Vec<_>>(),
        vec!["A".to_string(), "B".to_string(), "C".to_string()]
    );
}

#[test]
fn invariant_passes_when_when_does_not_match() {
    let inv = FieldInvariant {
        name: "LocalMustBeUnrestricted".into(),
        when: predicate(r#"{ field = "ConfigType", equals = "Local" }"#),
        require: predicate(r#"{ field = "NetworkingType", equals = "Unrestricted" }"#),
        message: None,
    };
    assert!(inv.passes(&json!({ "ConfigType": "Cloud" })));
    assert!(inv.passes(&json!({})));
}

#[test]
fn invariant_fails_when_require_does_not_match() {
    let inv = FieldInvariant {
        name: "LocalMustBeUnrestricted".into(),
        when: predicate(r#"{ field = "ConfigType", equals = "Local" }"#),
        require: predicate(r#"{ field = "NetworkingType", equals = "Unrestricted" }"#),
        message: None,
    };
    assert!(inv.passes(&json!({
        "ConfigType": "Local",
        "NetworkingType": "Unrestricted"
    })));
    assert!(!inv.passes(&json!({
        "ConfigType": "Local",
        "NetworkingType": "Limited"
    })));
    assert!(!inv.passes(&json!({ "ConfigType": "Local" })));
}

#[test]
fn has_empty_combinator_detects_nested() {
    let outer = predicate(
        r#"{ all_of = [
          { field = "A", equals = "x" },
          { any_of = [] },
        ]}"#,
    );
    assert!(outer.has_empty_combinator());

    let non_empty = predicate(r#"{ field = "X", absent = true }"#);
    assert!(!non_empty.has_empty_combinator());
}

#[test]
fn evaluate_on_non_object_returns_false_for_leaves() {
    let p = predicate(r#"{ field = "X", equals = "v" }"#);
    assert!(!p.evaluate(&json!(null)));
    assert!(!p.evaluate(&json!(42)));
    assert!(!p.evaluate(&json!("scalar")));
}

#[test]
fn mixed_leaf_and_combinator_is_rejected() {
    let msg = predicate_err(r#"{ field = "X", absent = true, any_of = [] }"#);
    assert!(msg.contains("mix combinator"), "error was: {msg}");
}

#[test]
fn mixed_operators_on_leaf_are_rejected() {
    let msg = predicate_err(r#"{ field = "X", absent = true, equals = "y" }"#);
    assert!(msg.contains("exactly one"), "error was: {msg}");
}

#[test]
fn leaf_without_operator_is_rejected() {
    let msg = predicate_err(r#"{ field = "X" }"#);
    assert!(msg.contains("exactly one"), "error was: {msg}");
}

#[test]
fn unknown_predicate_key_is_rejected() {
    let msg = predicate_err(r#"{ field = "X", wibble = true }"#);
    assert!(msg.contains("unknown predicate key"), "error was: {msg}");
}

#[test]
fn multiple_combinators_rejected() {
    let msg = predicate_err(r#"{ any_of = [], all_of = [] }"#);
    assert!(msg.contains("exactly one"), "error was: {msg}");
}

#[test]
fn field_invariant_deserializes_from_toml() {
    let src = r#"
name = "LocalMustBeUnrestricted"
when = { field = "ConfigType", equals = "Local" }
require = { field = "NetworkingType", equals = "Unrestricted" }
message = "Local environments must use Unrestricted networking"
"#;
    let inv: FieldInvariant = toml::from_str(src).expect("should parse");
    assert_eq!(inv.name, "LocalMustBeUnrestricted");
    assert_eq!(
        inv.message.as_deref(),
        Some("Local environments must use Unrestricted networking")
    );
    assert!(inv.passes(&json!({ "ConfigType": "Cloud" })));
    assert!(!inv.passes(&json!({
        "ConfigType": "Local",
        "NetworkingType": "Limited"
    })));
}
