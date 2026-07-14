use super::*;

const PREFIX: &str =
    "[automaton]\nname = \"SpanTest\"\nstates = [\"Ready\"]\ninitial = \"Ready\"\n";

#[test]
fn invariant_assertion_retains_exact_source_span() {
    let source = format!(
        "{PREFIX}\n[[invariant]]\nname = \"Unsupported\"\nwhen = [\"Ready\"]\n  assert = \"used_bytes <= quota_limit\" # safety claim\n"
    );
    let automaton = parse_toml_to_automaton(&source).expect("parse IOA");
    let span = automaton.invariants[0]
        .assert_span
        .expect("assertion source span");
    assert_eq!(
        &source[span.start.byte..span.end.byte],
        "used_bytes <= quota_limit"
    );
    assert_eq!((span.start.line, span.start.column), (9, 13));
    assert_eq!((span.end.line, span.end.column), (9, 38));
}

#[test]
fn single_quoted_invariant_retains_inner_source_span() {
    let source = "[automaton]\nname = 'Entity'\nstates = ['Active']\ninitial = 'Active'\n\n[[invariant]]\nname = 'Flag'\nassert = 'declared_flag'\n";
    let automaton = parse_toml_to_automaton(source).expect("parse IOA");
    let span = automaton.invariants[0]
        .assert_span
        .expect("assertion source span");
    assert_eq!(&source[span.start.byte..span.end.byte], "declared_flag");
}

#[test]
fn crlf_invariant_retains_inner_source_span() {
    let source = "[automaton]\r\nname = \"SpanTest\"\r\nstates = [\"Ready\"]\r\ninitial = \"Ready\"\r\n\r\n[[invariant]]\r\nname = \"Unsupported\"\r\nwhen = [\"Ready\"]\r\nassert = \"used_bytes <= quota_limit\"\r\n";
    let automaton = parse_toml_to_automaton(source).expect("parse IOA");
    let span = automaton.invariants[0]
        .assert_span
        .expect("assertion source span");
    assert_eq!(
        &source[span.start.byte..span.end.byte],
        "used_bytes <= quota_limit"
    );
    assert_eq!((span.start.line, span.start.column), (9, 11));
}

#[test]
fn repeated_invariants_receive_distinct_source_spans() {
    let source = format!(
        "{PREFIX}\n[[invariant]]\nname = \"First\"\nassert = \"value != ''\"\n\n[[invariant]]\nname = \"Second\"\nassert = \"value != ''\"\n"
    );
    let automaton = parse_toml_to_automaton(&source).expect("parse IOA");
    let first = automaton.invariants[0].assert_span.expect("first span");
    let second = automaton.invariants[1].assert_span.expect("second span");
    assert_ne!(first, second);
    assert_eq!(&source[first.start.byte..first.end.byte], "value != ''");
    assert_eq!(&source[second.start.byte..second.end.byte], "value != ''");
}

#[test]
fn parse_kv_preserves_quotes_inside_assertion() {
    let (key, value) = parse_kv("assert = \"goal != ''\"").expect("key/value");
    assert_eq!(key, "assert");
    assert_eq!(value, "goal != ''");
}

#[test]
fn parse_kv_ignores_inline_comment_after_quoted_value() {
    let (_, value) =
        parse_kv("assert = \"used_bytes <= quota_limit\" # safety claim").expect("key/value");
    assert_eq!(value, "used_bytes <= quota_limit");
}

#[test]
fn mixed_legacy_and_inline_effect_array_retains_both_effects() {
    let source = format!(
        "{PREFIX}\n[[state]]\nname = \"ready\"\ntype = \"bool\"\ninitial = \"false\"\n\n[[action]]\nname = \"Activate\"\nfrom = [\"Ready\"]\neffect = [\"set ready true\", {{ type = \"emit\", event = \"Activated\" }}]\n"
    );
    let automaton = parse_toml_to_automaton(&source).expect("parse IOA");
    assert_eq!(automaton.actions[0].effect.len(), 2);
    assert!(matches!(
        &automaton.actions[0].effect[0],
        Effect::SetBool { var, value: true } if var == "ready"
    ));
    assert!(matches!(
        &automaton.actions[0].effect[1],
        Effect::Emit { event } if event == "Activated"
    ));
}
