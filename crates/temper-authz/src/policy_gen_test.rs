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
            .expect("matrix renders");
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
            .expect("matrix renders");
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
            .expect("matrix renders");
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
            .expect("matrix renders");
    assert!(policy.contains("principal is Agent"));
    assert!(policy.contains("context.agentType == \"claude-code\""));
    assert!(policy.contains("context.agentTypeVerified == true"));
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
            .expect("matrix renders");
    assert!(policy.contains("context.role == \"operations_agent\""));
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
            .expect("matrix renders");
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
            .expect("matrix renders");
    assert!(policy.contains("context.agentType == \"openclaw\""));
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
            .expect("matrix renders");
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

fn matrix(
    principal: PrincipalScope,
    action: ActionScope,
    resource: ResourceScope,
    duration: DurationScope,
) -> PolicyScopeMatrix {
    PolicyScopeMatrix {
        principal,
        action,
        resource,
        duration,
        agent_type_value: None,
        role_value: None,
        session_id: None,
    }
}

/// Decide one request against generated text, with an optional session id.
fn decide(
    text: &str,
    principal: &str,
    action: &str,
    resource: &str,
    session_id: Option<&str>,
) -> cedar_policy::Decision {
    let policies = PolicySet::from_str(text).expect("generated text parses");
    let context = match session_id {
        Some(id) => cedar_policy::Context::from_pairs([(
            "sessionId".to_string(),
            cedar_policy::RestrictedExpression::new_string(id.to_string()),
        )])
        .expect("context builds"),
        None => cedar_policy::Context::empty(),
    };
    let request = cedar_policy::Request::new(
        EntityUid::from_str(principal).expect("principal uid"),
        entity_uid("Action", action).expect("action uid"),
        EntityUid::from_str(resource).expect("resource uid"),
        context,
        None,
    )
    .expect("request builds");
    cedar_policy::Authorizer::new()
        .is_authorized(&request, &policies, &cedar_policy::Entities::empty())
        .decision()
}

#[test]
fn an_action_name_with_cedar_syntax_is_one_action_literal() {
    let action = "read\",\n  resource\n);\npermit(\n  principal == Agent::\"agent-a\",\n  action == Action::\"read";
    let m = matrix(
        PrincipalScope::ThisAgent,
        ActionScope::ThisAction,
        ResourceScope::AnyOfType,
        DurationScope::Always,
    );
    let text = generate_cedar_from_matrix("agent-a", "Agent", action, "Order", "o-1", &m)
        .expect("any action name renders as a literal");

    assert_eq!(PolicySet::from_str(&text).unwrap().policies().count(), 1);
    // The permit covers exactly the stored action on any Order.
    let agent = r#"Agent::"agent-a""#;
    assert_eq!(
        decide(&text, agent, action, r#"Order::"o-2""#, None),
        cedar_policy::Decision::Allow
    );
    assert_eq!(
        decide(&text, agent, "read", r#"Invoice::"inv-1""#, None),
        cedar_policy::Decision::Deny
    );
    assert_eq!(
        decide(&text, agent, action, r#"Invoice::"inv-1""#, None),
        cedar_policy::Decision::Deny
    );
}

#[test]
fn a_session_value_with_quotes_is_one_condition_literal() {
    let session = "S1\" || true || \"";
    let mut m = matrix(
        PrincipalScope::ThisAgent,
        ActionScope::ThisAction,
        ResourceScope::ThisResource,
        DurationScope::Session,
    );
    m.session_id = Some(session.to_string());
    let text = generate_cedar_from_matrix("agent-a", "Agent", "Ship", "Order", "ord-1", &m)
        .expect("any session id renders as a literal");

    let agent = r#"Agent::"agent-a""#;
    let order = r#"Order::"ord-1""#;
    assert_eq!(
        decide(&text, agent, "Ship", order, Some(session)),
        cedar_policy::Decision::Allow
    );
    assert_eq!(
        decide(&text, agent, "Ship", order, Some("S2")),
        cedar_policy::Decision::Deny
    );
}

#[test]
fn an_entity_id_with_escapes_matches_only_itself() {
    let m = matrix(
        PrincipalScope::ThisAgent,
        ActionScope::ThisAction,
        ResourceScope::ThisResource,
        DurationScope::Always,
    );
    let text = generate_cedar_from_matrix("agent-a", "Agent", "Ship", "Order", r"\u{41}", &m)
        .expect("any id renders as a literal");
    let agent = r#"Agent::"agent-a""#;
    assert_eq!(
        decide(&text, agent, "Ship", r#"Order::"\\u{41}""#, None),
        cedar_policy::Decision::Allow
    );
    assert_eq!(
        decide(&text, agent, "Ship", r#"Order::"A""#, None),
        cedar_policy::Decision::Deny
    );
}

#[test]
fn a_type_that_is_not_a_cedar_type_name_generates_nothing() {
    let m = matrix(
        PrincipalScope::ThisAgent,
        ActionScope::ThisAction,
        ResourceScope::AnyOfType,
        DurationScope::Always,
    );
    for resource_type in [
        "Order,\n  resource\n);\npermit(principal, action",
        "",
        "Order Item",
    ] {
        assert!(
            generate_cedar_from_matrix("agent-a", "Agent", "read", resource_type, "o-1", &m)
                .is_err(),
            "{resource_type:?} must not render"
        );
    }
    assert!(
        generate_cedar_from_matrix("agent-a", "Agent Admin", "read", "Order", "o-1", &m).is_err()
    );
}

#[test]
fn a_matrix_missing_its_companion_value_generates_nothing() {
    let m = matrix(
        PrincipalScope::AgentsWithRole,
        ActionScope::ThisAction,
        ResourceScope::AnyOfType,
        DurationScope::Always,
    );
    assert!(generate_cedar_from_matrix("agent-a", "Agent", "read", "Order", "o-1", &m).is_err());
}
