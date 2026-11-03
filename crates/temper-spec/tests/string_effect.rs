use temper_spec::automaton::{legacy::migrate_source, parse_automaton};

const SPEC: &str = include_str!("../../../test-fixtures/specs/string_effect.ioa.toml");

#[test]
fn string_literal_assignment_parses_and_round_trips() {
    let automaton = parse_automaton(SPEC).expect("string literal effects should parse");
    let effects = &automaton.actions[1].effect;
    assert_eq!(effects[0].to_string(), "error = ''");
    assert_eq!(effects[1].to_string(), "reason = 'recovered'");
    let migrated = migrate_source(SPEC).unwrap();
    assert_eq!(
        parse_automaton(&migrated.source).unwrap().actions[1].effect,
        *effects
    );
}

#[test]
fn invalid_string_assignments_are_refused_with_a_reason() {
    for (assignment, reason) in [
        ("error = 7", "string literal"),
        ("error = true", "string literal"),
        ("error = reason", "string literal"),
        ("error = params.error", "string literal"),
        ("error += 'x'", "string literal"),
        ("missing = ''", "unknown state variable 'missing'"),
    ] {
        let source = SPEC.replace("error = ''", assignment);
        let error = parse_automaton(&source).unwrap_err().to_string();
        assert!(error.contains(reason), "{assignment}: {error}");
    }
    for (kind, initial) in [
        ("counter", "0"),
        ("bool", "false"),
        ("list", "[]"),
        ("int", "0"),
    ] {
        let source = SPEC.replace(
            "type = \"string\"\ninitial = \"\"",
            &format!("type = \"{kind}\"\ninitial = {initial}"),
        );
        let error = parse_automaton(&source).unwrap_err().to_string();
        assert!(
            error.contains("error") && error.contains("effect"),
            "{error}"
        );
    }
}

#[test]
fn legacy_set_string_preserves_literals_and_validates_types() {
    for literal in ["", "quote's \"text\"\nnext", "雪"] {
        let table = format!(
            "[{{ type = \"set_string\", var = \"error\", value = {} }}]",
            toml::Value::String(literal.into())
        );
        let source = SPEC.replace("[\"error = ''\", \"reason = 'recovered'\"]", &table);
        let migrated = migrate_source(&source).unwrap();
        let parsed = parse_automaton(&migrated.source).unwrap();
        let expected = temper_spec::predicate::parse_effect(&format!(
            "error = {}",
            temper_spec::predicate::Literal::Str(literal.into())
        ))
        .unwrap();
        assert_eq!(parsed.actions[1].effect[0], expected);
    }
    for (var, value, reason) in [
        ("error", "false", "string"),
        ("missing", "\"\"", "declared string"),
        ("error", "0", "string"),
    ] {
        let source = SPEC.replace(
            "[\"error = ''\", \"reason = 'recovered'\"]",
            &format!("[{{ type = \"set_string\", var = \"{var}\", value = {value} }}]"),
        );
        let error = migrate_source(&source).unwrap_err();
        assert!(error.contains(reason), "{error}");
    }
}
