use super::*;

#[test]
fn semantic_actions_reject_ambiguous_duplicate_declarations() {
    let source = r#"
[automaton]
name = "Case"
states = ["Open", "Closed"]
initial = "Open"
allow_indefinite_states = ["Open", "Closed"]
[[action]]
name = "Change"
from = ["Open"]
to = "Closed"
[[action]]
name = "Change"
from = ["Closed"]
to = "Open"
"#;
    let parse = |input: &str| {
        crate::automaton::parse_automaton_with_liveness(
            input,
            crate::automaton::LivenessEnforcement::WarnOnly,
        )
    };
    assert_eq!(parse(source).unwrap().actions.len(), 2);
    let guard = r#"guard = [{ type = "system_one", model = "jev-latest", state = "message", questions = { ok = { type = "noul", instructions = "Should this action proceed?" } }, assert = "answers.ok.noul >= 0.8" }]"#;
    let guarded = source.replace("to = \"Closed\"", &format!("to = \"Closed\"\n{guard}"));
    let error = parse(&guarded).unwrap_err().to_string();
    assert!(error.contains("requires a unique declaration"), "{error}");
}

fn noul(assertion: &str) -> SystemOneGuard {
    serde_json::from_value(json!({
        "model":"jev-latest", "state":{"ref":"entity.Messages"},
        "questions":{"human_requested":{"type":"noul","instructions":"Has a human been requested?"}},
        "assert":assertion
    })).unwrap()
}

fn response(noul: f64) -> Value {
    json!({"model":"jev-test","answers":{"human_requested":{"type":"noul","noul":noul}}})
}

#[test]
fn noul_threshold_and_deterministic_rounding() {
    let guard = noul("answers.human_requested.noul >= 0.8");
    assert!(guard.evaluate_response(&response(0.8)).unwrap());
    assert!(!guard.evaluate_response(&response(0.799)).unwrap());
    assert!(guard.evaluate_response(&response(0.7999999996)).unwrap());
    assert!(!guard.evaluate_response(&response(0.7999999994)).unwrap());
}

#[test]
fn malformed_or_incomplete_answers_never_enable_guard() {
    let guard = noul("answers.human_requested.noul >= 0.8");
    for invalid in [
        json!({"model":"jev-test","answers":{}}),
        json!({"model":"jev-test","answers":{"human_requested":{"type":"score","noul":0.9}}}),
        json!({"model":"jev-test","answers":{"human_requested":{"type":"noul","noul":"0.9"}}}),
        json!({"model":"jev-test","answers":{"human_requested":{"type":"noul","noul":1.2}}}),
        json!({"model":"jev-test","answers":{"human_requested":{"type":"noul","noul":1.0000000001}}}),
        json!({"model":"jev-test","answers":{"human_requested":{"type":"noul","noul":-0.0000000001}}}),
        json!({"model":"jev-test","answers":{"human_requested":{"type":"noul","noul":0.9,"confidence":1}}}),
        json!({"answers":{"human_requested":{"type":"noul","noul":0.9}}}),
    ] {
        assert!(guard.evaluate_response(&invalid).is_err(), "{invalid}");
    }
}

#[test]
fn bindings_capture_only_declared_fields_preserving_json_structure() {
    let mut guard = noul("answers.human_requested.noul >= 0.8");
    guard.state =
        json!({"chat":{"ref":"entity.Messages"},"context":[{"ref":"params.Message"},"literal",4]});
    let entity = json!({"Messages":[{"role":"user","text":"Help"}],"secret":"must not be sent"});
    let params = json!({"Message":"I want a human"});
    assert_eq!(
        guard.resolve_state(&entity, &params).unwrap(),
        json!({
            "chat":[{"role":"user","text":"Help"}],"context":["I want a human","literal",4]
        })
    );
    assert!(guard.resolve_state(&json!({}), &params).is_err());
    guard
        .validate_bindings(
            &BTreeSet::from(["Messages".into()]),
            &BTreeSet::from(["Message".into()]),
        )
        .unwrap();
    assert!(
        guard
            .validate_bindings(&BTreeSet::new(), &BTreeSet::from(["Message".into()]))
            .is_err()
    );
    let request = guard.request(guard.resolve_state(&entity, &params).unwrap());
    assert!(request.get("assert").is_none());
    assert!(!request.to_string().contains("must not be sent"));
}

#[test]
fn binding_typos_and_ambiguous_objects_reject() {
    let mut guard = noul("answers.human_requested.noul >= 0.8");
    for invalid in [
        json!({"ref":"env.KEY"}),
        json!({"ref":"entity.Messages","other":true}),
        json!({"ref":"params."}),
    ] {
        guard.state = invalid;
        assert!(guard.validate().is_err());
    }
}

#[test]
fn reference_fanout_cannot_exceed_the_resolved_context_byte_budget() {
    let mut guard = noul("answers.human_requested.noul >= 0.8");
    guard.state = Value::Array(vec![json!({"ref":"entity.Messages"}); 64]);
    let entity = json!({"Messages":"a".repeat(1_024)});
    guard.validate().unwrap();
    assert!(
        guard.resolve_state(&entity, &json!({})).is_err(),
        "reference fanout expanded past the 64 KiB context budget"
    );
    guard.state = json!({"ref":"entity.Messages"});
    assert_eq!(
        guard.resolve_state(&entity, &json!({})).unwrap(),
        entity["Messages"]
    );
}

#[test]
fn resolved_context_budget_counts_json_escaping_and_container_overhead() {
    let mut guard = noul("answers.human_requested.noul >= 0.8");
    let byte_budget = 64 * 1_024;
    let exact = json!({"Messages":"a".repeat(byte_budget - 2)});
    assert_eq!(
        guard.resolve_state(&exact, &json!({})).unwrap(),
        exact["Messages"]
    );
    let oversized = json!({"Messages":"a".repeat(byte_budget - 1)});
    assert!(guard.resolve_state(&oversized, &json!({})).is_err());
    let escaped = json!({"Messages":"\\".repeat(byte_budget / 2)});
    assert!(guard.resolve_state(&escaped, &json!({})).is_err());
    guard.state = json!([{"ref":"params.Message"}, {"ref":"params.Message"}]);
    assert!(
        guard
            .resolve_state(
                &json!({}),
                &json!({"Message":"a".repeat(byte_budget / 2 - 2)})
            )
            .is_err()
    );
}

#[test]
fn answer_reference_types_are_validated_at_spec_load() {
    for assertion in [
        "answers.missing.noul >= 0.8",
        "answers.human_requested.confidence >= 0.8",
        "answers.human_requested.noul >= true",
        "answers.human_requested.noul >= 0.8 || true",
        "answers.human_requested.noul >= 0.8 &&",
        "entity.Body == \"yes\"",
    ] {
        assert!(noul(assertion).validate().is_err(), "{assertion}");
    }
}

#[test]
fn conjunctions_preserve_answer_consistency_for_verification() {
    let impossible =
        noul("answers.human_requested.noul >= 0.8 && answers.human_requested.noul < 0.2");
    impossible.validate().unwrap();
    assert!(!impossible.assertion_may_hold().unwrap());
    let boundary =
        noul("answers.human_requested.noul >= 0.8 && answers.human_requested.noul <= 0.8");
    assert!(boundary.assertion_may_hold().unwrap());
    assert!(boundary.evaluate_response(&response(0.8)).unwrap());
    assert!(
        !noul("answers.human_requested.noul > 1")
            .assertion_may_hold()
            .unwrap()
    );
}

#[test]
fn choice_native_response_and_confidence() {
    let guard: SystemOneGuard = serde_json::from_value(json!({
        "model":"jev-latest", "state":"My invoice is wrong",
        "questions":{"department":{"type":"choice","instructions":{"task":"Pick a team"},"criteria":{"billing":"Invoices","technical":null}}},
        "assert":"answers.department.choice == \"billing\" && answers.department.confidence >= 0.7"
    })).unwrap();
    let response = json!({"model":"jev-test","answers":{"department":{"type":"choice","choice":"billing","confidence":0.8,"probabilities":{"billing":0.9,"technical":0.1}}}});
    assert!(guard.evaluate_response(&response).unwrap());
    let mut invalid = response.clone();
    invalid["answers"]["department"]["choice"] = json!("technical");
    assert!(guard.evaluate_response(&invalid).is_err());
    invalid = response.clone();
    invalid["answers"]["department"]["probabilities"]["billing"] = json!(0.6);
    assert!(guard.evaluate_response(&invalid).is_err());
    let mut contradiction = guard.clone();
    contradiction.assertion =
        "answers.department.choice == \"billing\" && answers.department.choice == \"technical\""
            .into();
    assert!(!contradiction.assertion_may_hold().unwrap());
}

#[test]
fn choice_literals_can_contain_comparison_and_conjunction_characters() {
    let guard: SystemOneGuard = serde_json::from_value(json!({
        "model":"jev-latest","state":"text",
        "questions":{"q":{"type":"choice","instructions":"Select","criteria":{"a && b > c":"Literal","other":"Other"}}},
        "assert":"answers.q.choice == \"a && b > c\""
    })).unwrap();
    assert!(guard.validate().is_ok());
    assert!(guard.assertion_may_hold().unwrap());
}

#[test]
fn score_native_response_fractional_value_and_rubric() {
    let guard: SystemOneGuard = serde_json::from_value(json!({
        "model":"jev-latest","state":"Work is blocked",
        "questions":{"urgency":{"type":"score","instructions":["Evaluate impact"],"criteria":["Calm","Frustrated","Very angry"]}},
        "assert":"answers.urgency.score >= 1.5"
    })).unwrap();
    let response = json!({"model":"jev-test","answers":{"urgency":{
        "type":"score","score":1.6,"confidence":0.78,
        "legend":{"0":"Calm","1":"Frustrated","2":"Very angry"},
        "probabilities":{"0":0.05,"1":0.3,"2":0.65}
    }}});
    assert!(guard.evaluate_response(&response).unwrap());
    let mut invalid = response.clone();
    invalid["answers"]["urgency"]["score"] = json!(2.1);
    assert!(guard.evaluate_response(&invalid).is_err());
    invalid = response.clone();
    invalid["answers"]["urgency"]["legend"]["2"] = json!("Changed rubric");
    assert!(guard.evaluate_response(&invalid).is_err());
}

#[test]
fn native_noul_criteria_and_unknown_fields() {
    let mut value = serde_json::to_value(noul("answers.human_requested.noul >= 0.8")).unwrap();
    value["questions"]["human_requested"]["criteria"] =
        json!({"true":"Explicit request","false":"No request"});
    serde_json::from_value::<SystemOneGuard>(value.clone())
        .unwrap()
        .validate()
        .unwrap();
    value["questions"]["human_requested"]["extra"] = json!(true);
    assert!(serde_json::from_value::<SystemOneGuard>(value).is_err());
}

#[test]
fn guard_identity_changes_when_any_declared_input_changes() {
    let guard = noul("answers.human_requested.noul >= 0.8");
    assert_eq!(guard.key(), guard.clone().key());
    let mut changed = guard.clone();
    changed.model = "jev-next".into();
    assert_ne!(guard.key(), changed.key());
    changed = guard.clone();
    changed.state = json!({"ref":"entity.Subject"});
    assert_ne!(guard.key(), changed.key());
    assert!(guard.key().starts_with("__system_one:"));
}

#[test]
fn decimal_scientific_notation_and_rounding_are_integer_defined() {
    use super::decimal::fixed;
    assert_eq!(fixed("8e-1").unwrap(), 800_000_000);
    assert_eq!(fixed("0.0000000005").unwrap(), 1);
    assert_eq!(fixed("-0.0000000005").unwrap(), -1);
    assert_eq!(fixed("1e-40").unwrap(), 0);
    assert!(fixed("NaN").is_err());
    assert!(fixed("1e999").is_err());
}

#[test]
fn inline_guard_list_handles_nested_questions_comments_and_literal_brackets() {
    let source = r#"
[automaton]
name = "Review"
states = ["Open", "Escalated"]
initial = "Open"
[[action]]
name = "Escalate"
from = ["Open"]
to = "Escalated"
guard = [
  # A local precondition alongside API-shaped nested questions.
  { type = "is_true", var = "assigned" },
  { type = "system_one", model = "jev-latest", state = { chat = { ref = "entity.Messages" } }, questions = { urgent = { type = "noul", instructions = "Does text [convey urgency]?" }, priority = { type = "score", instructions = "Impact?", criteria = ["Normal", "Degraded", "Blocked"] } }, assert = "answers.urgent.noul >= 0.8 && answers.priority.score >= 1.5" },
]
"#;
    let automaton = super::super::parse_automaton(source).unwrap();
    assert_eq!(automaton.actions[0].guard.len(), 2);
    assert!(matches!(
        automaton.actions[0].guard[1],
        super::super::Guard::SystemOne(_)
    ));
    assert!(
        super::super::parse_automaton(&source.replace(
            "model = \"jev-latest\"",
            "model = \"jev-latest\", typo = true"
        ))
        .is_err()
    );
}
