//! Tests for [`super`] Cedar policy generation, including ARN-172 injection
//! resistance. Kept in a sibling file so `policy_gen.rs` stays focused.

use super::*;

#[test]
fn test_this_agent_this_action_this_resource() {
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::ThisAgent,
        action: ActionScope::ThisAction,
        resource: ResourceScope::ThisResource,
        duration: DurationScope::Always,
        agent_type_value: None,
        role_value: None,
        session_id: None,
    };
    let policy =
        generate_cedar_from_matrix("bot-1", "Agent", "submitOrder", "Order", "order-123", &m)
            .expect("valid policy");
    assert!(policy.contains("principal == Agent::\"bot-1\""));
    assert!(policy.contains("action == Action::\"submitOrder\""));
    assert!(policy.contains("resource == Order::\"order-123\""));
    assert!(!policy.contains("when"));
}

#[test]
fn test_this_agent_this_action_any_of_type() {
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::ThisAgent,
        action: ActionScope::ThisAction,
        resource: ResourceScope::AnyOfType,
        duration: DurationScope::Always,
        agent_type_value: None,
        role_value: None,
        session_id: None,
    };
    let policy =
        generate_cedar_from_matrix("bot-1", "Agent", "submitOrder", "Order", "order-123", &m)
            .expect("valid policy");
    assert!(policy.contains("resource is Order"));
}

#[test]
fn test_any_agent_all_actions_any_resource() {
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::AnyAgent,
        action: ActionScope::AllActions,
        resource: ResourceScope::AnyResource,
        duration: DurationScope::Always,
        agent_type_value: None,
        role_value: None,
        session_id: None,
    };
    let policy =
        generate_cedar_from_matrix("bot-1", "Agent", "submitOrder", "Order", "order-123", &m)
            .expect("valid policy");
    assert!(policy.contains("principal is Agent"));
    assert!(!policy.contains("Action::"));
    assert!(!policy.contains("Order"));
}

#[test]
fn test_agents_of_type_condition() {
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::AgentsOfType,
        action: ActionScope::ThisAction,
        resource: ResourceScope::AnyOfType,
        duration: DurationScope::Always,
        agent_type_value: Some("claude-code".to_string()),
        role_value: None,
        session_id: None,
    };
    let policy =
        generate_cedar_from_matrix("bot-1", "Agent", "submitOrder", "Order", "order-123", &m)
            .expect("valid policy");
    assert!(policy.contains("principal is Agent"));
    assert!(policy.contains("principal.agent_type == \"claude-code\""));
    assert!(policy.contains("principal.agentTypeVerified == true"));
}

#[test]
fn test_agents_with_role_condition() {
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::AgentsWithRole,
        action: ActionScope::ThisAction,
        resource: ResourceScope::AnyOfType,
        duration: DurationScope::Always,
        agent_type_value: None,
        role_value: Some("operations_agent".to_string()),
        session_id: None,
    };
    let policy =
        generate_cedar_from_matrix("bot-1", "Agent", "submitOrder", "Order", "order-123", &m)
            .expect("valid policy");
    assert!(policy.contains("principal.role == \"operations_agent\""));
    assert!(policy.contains("principal.agentTypeVerified == true"));
}

#[test]
fn test_session_duration_adds_session_id() {
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::ThisAgent,
        action: ActionScope::ThisAction,
        resource: ResourceScope::AnyOfType,
        duration: DurationScope::Session,
        agent_type_value: None,
        role_value: None,
        session_id: Some("sess-abc".to_string()),
    };
    let policy =
        generate_cedar_from_matrix("bot-1", "Agent", "submitOrder", "Order", "order-123", &m)
            .expect("valid policy");
    assert!(policy.contains("context.sessionId == \"sess-abc\""));
}

#[test]
fn test_combined_agent_type_and_session() {
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::AgentsOfType,
        action: ActionScope::ThisAction,
        resource: ResourceScope::AnyOfType,
        duration: DurationScope::Session,
        agent_type_value: Some("openclaw".to_string()),
        role_value: None,
        session_id: Some("sess-xyz".to_string()),
    };
    let policy =
        generate_cedar_from_matrix("bot-1", "Agent", "submitOrder", "Order", "order-123", &m)
            .expect("valid policy");
    assert!(policy.contains("principal.agent_type == \"openclaw\""));
    assert!(policy.contains("context.sessionId == \"sess-xyz\""));
}

#[test]
fn test_all_actions_on_type_still_constrains_resource() {
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::ThisAgent,
        action: ActionScope::AllActionsOnType,
        resource: ResourceScope::AnyOfType,
        duration: DurationScope::Always,
        agent_type_value: None,
        role_value: None,
        session_id: None,
    };
    let policy =
        generate_cedar_from_matrix("bot-1", "Agent", "submitOrder", "Order", "order-123", &m)
            .expect("valid policy");
    assert!(policy.contains("resource is Order"));
    assert!(!policy.contains("Action::"));
}

#[test]
fn all_actions_on_type_cannot_be_widened_by_any_resource() {
    let matrix = PolicyScopeMatrix {
        principal: PrincipalScope::ThisAgent,
        action: ActionScope::AllActionsOnType,
        resource: ResourceScope::AnyResource,
        duration: DurationScope::Always,
        agent_type_value: None,
        role_value: None,
        session_id: None,
    };

    let policy = generate_cedar_from_matrix(
        "bot-1",
        "Agent",
        "submitOrder",
        "Order",
        "order-123",
        &matrix,
    )
    .expect("valid policy");

    assert!(policy.contains("resource is Order"));
    assert!(!policy.contains("Action::"));
}

#[test]
fn test_default_matrix() {
    let m = PolicyScopeMatrix::default_for(Some("claude-code"));
    assert_eq!(m.principal, PrincipalScope::ThisAgent);
    assert_eq!(m.action, ActionScope::ThisAction);
    assert_eq!(m.resource, ResourceScope::AnyOfType);
    assert_eq!(m.duration, DurationScope::Always);
    assert_eq!(m.agent_type_value, Some("claude-code".to_string()));
}

#[test]
fn validate_matrix_requires_agent_type_for_agents_of_type() {
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::AgentsOfType,
        action: ActionScope::ThisAction,
        resource: ResourceScope::AnyOfType,
        duration: DurationScope::Always,
        agent_type_value: None,
        role_value: None,
        session_id: None,
    };
    assert!(validate_policy_scope_matrix(&m).is_err());
}

#[test]
fn validate_matrix_requires_role_for_agents_with_role() {
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::AgentsWithRole,
        action: ActionScope::ThisAction,
        resource: ResourceScope::AnyOfType,
        duration: DurationScope::Always,
        agent_type_value: None,
        role_value: None,
        session_id: None,
    };
    assert!(validate_policy_scope_matrix(&m).is_err());
}

#[test]
fn validate_matrix_requires_session_for_session_duration() {
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::ThisAgent,
        action: ActionScope::ThisAction,
        resource: ResourceScope::AnyOfType,
        duration: DurationScope::Session,
        agent_type_value: None,
        role_value: None,
        session_id: None,
    };
    assert!(validate_policy_scope_matrix(&m).is_err());
}

#[test]
fn generation_fails_closed_when_scope_companion_is_missing() {
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::AgentsOfType,
        action: ActionScope::ThisAction,
        resource: ResourceScope::AnyOfType,
        duration: DurationScope::Always,
        agent_type_value: None,
        role_value: None,
        session_id: None,
    };

    let result = generate_cedar_from_matrix("bot-1", "Agent", "act", "Order", "ord-1", &m);
    assert!(
        result.is_err(),
        "invalid scope must not produce a broad permit"
    );
}

// --------------------------------------------------------------------------
// ARN-172: injection resistance. Agent-influenced ids/actions must be
// confined to a single Cedar string literal — never able to add, remove, or
// re-scope a policy, and never able to break the generated policy so a
// whole-tenant reload fails.
// --------------------------------------------------------------------------

fn this_resource_matrix() -> PolicyScopeMatrix {
    PolicyScopeMatrix {
        principal: PrincipalScope::ThisAgent,
        action: ActionScope::ThisAction,
        resource: ResourceScope::ThisResource,
        duration: DurationScope::Always,
        agent_type_value: None,
        role_value: None,
        session_id: None,
    }
}

fn m_ok() -> PolicyScopeMatrix {
    this_resource_matrix()
}

/// The generated text must always parse to exactly the one permit the human
/// approved — return it so each test can assert on its constraints.
fn parse_single_policy(text: &str) -> cedar_policy::Policy {
    let set: cedar_policy::PolicySet = text
        .parse()
        .unwrap_or_else(|e| panic!("generated policy must be valid Cedar: {e}\n---\n{text}"));
    let policies: Vec<_> = set.policies().cloned().collect();
    assert_eq!(
        policies.len(),
        1,
        "exactly one permit expected; injection added/removed policies:\n{text}"
    );
    policies.into_iter().next().unwrap()
}

fn expect_resource_eq(policy: &cedar_policy::Policy, type_name: &str, id: &str) {
    use std::str::FromStr;
    let expected = cedar_policy::EntityUid::from_type_name_and_id(
        cedar_policy::EntityTypeName::from_str(type_name).unwrap(),
        cedar_policy::EntityId::new(id),
    );
    match policy.resource_constraint() {
        cedar_policy::ResourceConstraint::Eq(uid) => assert_eq!(
            uid, expected,
            "resource id did not round-trip to the exact untrusted value"
        ),
        other => panic!("expected resource == {type_name}::\"...\", got {other:?}"),
    }
}

/// Proves a `when`-condition string literal round-trips to the *exact*
/// untrusted value. Cedar parses the generated text, then re-serializes it to
/// its EST JSON where the literal appears as a plain JSON string — so a match
/// here means the value was neither truncated at an injected quote nor
/// otherwise mangled by escaping. Independent of the generator's own escaping
/// routine (text -> Cedar parser -> EST -> compare).
fn condition_string_round_trips(policy: &cedar_policy::Policy, needle: &str) -> bool {
    fn walk(v: &serde_json::Value, needle: &str) -> bool {
        match v {
            serde_json::Value::String(s) => s == needle,
            serde_json::Value::Array(a) => a.iter().any(|x| walk(x, needle)),
            serde_json::Value::Object(o) => o.values().any(|x| walk(x, needle)),
            _ => false,
        }
    }
    let json = policy.to_json().expect("policy serializes to EST JSON");
    walk(&json, needle)
}

#[test]
fn injected_quote_in_resource_id_cannot_add_policies() {
    // Classic breakout: close the string, close the permit, smuggle a broad
    // grant, comment out the tail.
    let evil = r#"x") ; permit(principal, action, resource); //"#;
    let text = generate_cedar_from_matrix("bot-1", "Agent", "act", "Order", evil, &m_ok())
        .expect("benign type names should generate a valid policy");
    let policy = parse_single_policy(&text);
    expect_resource_eq(&policy, "Order", evil);
}

#[test]
fn injected_backslash_in_resource_id_does_not_break_reload() {
    // A value ending in `\` escapes the closing quote under naive formatting,
    // leaving the literal unterminated and failing the whole tenant reload.
    let evil = r#"C:\"#;
    let text = generate_cedar_from_matrix("bot-1", "Agent", "act", "Order", evil, &m_ok())
        .expect("benign type names should generate a valid policy");
    let policy = parse_single_policy(&text);
    expect_resource_eq(&policy, "Order", evil);
}

#[test]
fn injected_quote_and_backslash_in_agent_and_action_round_trip() {
    let bad_agent = r#"a"\b"#;
    let bad_action = r#"do") ; forbid(principal, action, resource); //"#;
    let text =
        generate_cedar_from_matrix(bad_agent, "Agent", bad_action, "Order", "ord-1", &m_ok())
            .expect("benign type names should generate a valid policy");
    let policy = parse_single_policy(&text);
    expect_resource_eq(&policy, "Order", "ord-1");
}

#[test]
fn injected_value_in_session_condition_stays_confined() {
    let evil_session = r#"s" } ; permit(principal, action, resource); //"#;
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::ThisAgent,
        action: ActionScope::ThisAction,
        resource: ResourceScope::AnyOfType,
        duration: DurationScope::Session,
        agent_type_value: None,
        role_value: None,
        session_id: Some(evil_session.to_string()),
    };
    let text = generate_cedar_from_matrix("bot-1", "Agent", "act", "Order", "ord-1", &m)
        .expect("benign type names should generate a valid policy");
    // Confinement: exactly one policy (no breakout) AND the exact value
    // survives inside the condition (no mangling / truncation).
    let policy = parse_single_policy(&text);
    assert!(
        condition_string_round_trips(&policy, evil_session),
        "session id did not round-trip exactly into the when-condition:\n{text}"
    );
}

#[test]
fn injected_value_in_agent_type_condition_stays_confined() {
    let evil_type = r#"t" } ; permit(principal, action, resource); //"#;
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::AgentsOfType,
        action: ActionScope::ThisAction,
        resource: ResourceScope::AnyOfType,
        duration: DurationScope::Always,
        agent_type_value: Some(evil_type.to_string()),
        role_value: None,
        session_id: None,
    };
    let text = generate_cedar_from_matrix("bot-1", "Agent", "act", "Order", "ord-1", &m)
        .expect("benign type names should generate a valid policy");
    let policy = parse_single_policy(&text);
    assert!(
        condition_string_round_trips(&policy, evil_type),
        "agentType did not round-trip exactly into the when-condition:\n{text}"
    );
}

#[test]
fn injected_value_in_role_condition_stays_confined() {
    let evil_role = r#"r" } ; permit(principal, action, resource); //"#;
    let m = PolicyScopeMatrix {
        principal: PrincipalScope::AgentsWithRole,
        action: ActionScope::ThisAction,
        resource: ResourceScope::AnyOfType,
        duration: DurationScope::Always,
        agent_type_value: None,
        role_value: Some(evil_role.to_string()),
        session_id: None,
    };
    let text = generate_cedar_from_matrix("bot-1", "Agent", "act", "Order", "ord-1", &m)
        .expect("benign type names should generate a valid policy");
    let policy = parse_single_policy(&text);
    assert!(
        condition_string_round_trips(&policy, evil_role),
        "role did not round-trip exactly into the when-condition:\n{text}"
    );
}

#[test]
fn injected_resource_type_identifier_fails_closed() {
    // The type-name position is a Cedar identifier, not a string literal — it
    // cannot be escaped, so an injected value must be rejected (fail closed)
    // rather than silently producing a broken/wider policy.
    let evil_type = r#"Order" ; permit(principal, action, resource) //"#;
    let result = generate_cedar_from_matrix("bot-1", "Agent", "act", evil_type, "ord-1", &m_ok());
    assert!(
        result.is_err(),
        "non-identifier resource type must fail closed, got: {result:?}"
    );
}

#[test]
fn injected_principal_kind_identifier_fails_closed() {
    let evil_kind = r#"Agent" ; permit(principal, action, resource) //"#;
    let result = generate_cedar_from_matrix("bot-1", evil_kind, "act", "Order", "ord-1", &m_ok());
    assert!(
        result.is_err(),
        "non-identifier principal kind must fail closed, got: {result:?}"
    );
}
