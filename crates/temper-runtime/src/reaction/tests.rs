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
        },
        resolve_target: TargetResolver::SameId,
    }
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
fn test_parse_empty() {
    assert!(parse_reactions("").unwrap().is_empty());
}
