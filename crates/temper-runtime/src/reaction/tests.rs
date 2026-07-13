use super::*;

fn rule(from_type: &str, emit: &str, to_type: &str, to_action: &str) -> ReactionRule {
    ReactionRule {
        name: format!("{from_type}_{emit}_to_{to_type}"),
        when: ReactionTrigger {
            entity_type: from_type.to_string(),
            action: Some(emit.to_string()),
            to_state: None,
        },
        then: ReactionTarget {
            entity_type: to_type.to_string(),
            action: to_action.to_string(),
            params: serde_json::Value::Null,
            params_from: BTreeMap::new(),
        },
        resolve_target: TargetResolver::SameId,
    }
}

fn wildcard_rule(from_type: &str, to_type: &str, to_action: &str) -> ReactionRule {
    let mut reaction = rule(from_type, "wildcard", to_type, to_action);
    reaction.when.action = None;
    reaction
}

#[test]
fn test_lookup_exact() {
    let mut reg = ReactionRegistry::new();
    reg.register(vec![rule(
        "Agent",
        "PrepareContext",
        "ContextManager",
        "PrepareContext",
    )]);
    let results = reg.lookup("Agent", "PrepareContext", "");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].then.entity_type, "ContextManager");
}

#[test]
fn test_no_match() {
    let reg = ReactionRegistry::new();
    assert!(reg.lookup("Agent", "PrepareContext", "").is_empty());
}

#[test]
fn literal_star_action_does_not_duplicate_wildcard_rule() {
    let mut registry = ReactionRegistry::new();
    registry.register(vec![wildcard_rule("Agent", "Target", "Run")]);

    let matches = registry.lookup("Agent", "*", "");
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0].then.entity_type, "Target");
}

#[test]
fn delimiter_characters_do_not_alias_actor_and_action_keys() {
    let mut registry = ReactionRegistry::new();
    registry.register(vec![
        rule("A:B", "C", "FirstTarget", "Run"),
        rule("A", "B:C", "SecondTarget", "Run"),
    ]);

    let first = registry.lookup("A:B", "C", "");
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].then.entity_type, "FirstTarget");
    let second = registry.lookup("A", "B:C", "");
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].then.entity_type, "SecondTarget");
    assert_eq!(registry.rules_for_actor_count("A:B"), 1);
    assert_eq!(registry.rules_for_actor_count("A"), 1);
}

#[test]
fn test_parse_reactions_toml() {
    let toml = r#"
[[reaction]]
name = "agent_requests_context"
[reaction.when]
entity_type = "Agent"
action = "StartProcess"
[reaction.then]
entity_type = "ContextManager"
action = "PrepareContext"
[reaction.resolve_target]
type = "SameId"
"#;
    let rules = parse_reactions(toml).unwrap();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].name, "agent_requests_context");
    assert_eq!(rules[0].then.entity_type, "ContextManager");
    assert_eq!(rules[0].resolve_target, TargetResolver::SameId);
}

#[test]
fn parse_reactions_preserves_declared_static_and_source_params() {
    let toml = r#"
[[reaction]]
name = "agent_invokes_llm"
[reaction.when]
entity_type = "Process"
action = "invoke_llm"
[reaction.then]
entity_type = "LlmIntegration"
action = "invoke_llm"
params = { mode = "chat" }
params_from = { prompt = "user_prompt" }
[reaction.resolve_target]
type = "SameId"
"#;

    let rules = parse_reactions(toml).expect("declared reaction params must parse");
    assert_eq!(rules[0].then.params, serde_json::json!({"mode": "chat"}));
    assert_eq!(
        rules[0].then.params_from.get("prompt").map(String::as_str),
        Some("user_prompt")
    );
}

#[test]
fn parse_reactions_rejects_static_and_source_param_collision() {
    let toml = r#"
[[reaction]]
name = "ambiguous"
[reaction.when]
entity_type = "Source"
action = "Changed"
[reaction.then]
entity_type = "Target"
action = "Receive"
params = { shared = "static" }
params_from = { shared = "source_field" }
[reaction.resolve_target]
type = "SameId"
"#;

    let error = parse_reactions(toml).expect_err("ambiguous parameter source must be rejected");
    assert!(error.contains("appears in both params and params_from"));
}

#[test]
fn test_parse_empty() {
    assert!(parse_reactions("").unwrap().is_empty());
}
