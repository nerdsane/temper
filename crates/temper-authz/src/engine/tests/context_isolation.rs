use super::*;

#[test]
fn resource_fields_cannot_spoof_verified_agent_type() {
    let policy = r#"
        permit(principal is Agent, action == Action::"read", resource is Doc)
        when {
            principal.agent_type == "trusted-worker" &&
            principal.agentTypeVerified == true
        };
    "#;
    let engine = AuthzEngine::new(policy).unwrap();
    let ctx = SecurityContext::from_headers(&[
        ("X-Temper-Principal-Id".to_string(), "attacker".to_string()),
        ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
    ])
    .with_agent_context(Some("attacker"), None, Some("untrusted-worker"));
    let attrs = HashMap::from([
        ("id".to_string(), serde_json::json!("doc-1")),
        ("agentType".to_string(), serde_json::json!("trusted-worker")),
        ("agentTypeVerified".to_string(), serde_json::json!(true)),
        (
            "agent_type".to_string(),
            serde_json::json!("trusted-worker"),
        ),
    ]);

    let decision = engine.authorize(&ctx, "read", "Doc", &attrs);
    assert!(
        !decision.is_allowed(),
        "resource fields must not replace verified principal identity: {decision:?}"
    );
}

#[test]
fn resource_fields_cannot_spoof_verified_agent_role() {
    let policy = r#"
        permit(principal is Agent, action == Action::"read", resource is Doc)
        when {
            principal.role == "operator" &&
            principal.agentTypeVerified == true
        };
    "#;
    let engine = AuthzEngine::new(policy).unwrap();
    let mut ctx = SecurityContext::from_headers(&[
        ("X-Temper-Principal-Id".to_string(), "attacker".to_string()),
        ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
        ("X-Temper-Agent-Role".to_string(), "viewer".to_string()),
    ])
    .with_agent_context(Some("attacker"), None, Some("worker"));
    ctx.principal
        .attributes
        .insert("role".to_string(), serde_json::json!("operator"));
    let attrs = HashMap::from([
        ("id".to_string(), serde_json::json!("doc-1")),
        ("role".to_string(), serde_json::json!("operator")),
        ("agentTypeVerified".to_string(), serde_json::json!(true)),
    ]);

    let decision = engine.authorize(&ctx, "read", "Doc", &attrs);
    assert!(
        !decision.is_allowed(),
        "resource/arbitrary principal fields must not replace canonical role or verification"
    );
}

#[test]
fn resource_fields_cannot_spoof_request_session() {
    let policy = r#"
        permit(
            principal == Agent::"agent-1",
            action == Action::"read",
            resource is Doc
        ) when { context.sessionId == "approved-session" };
    "#;
    let engine = AuthzEngine::new(policy).unwrap();
    let ctx = SecurityContext::from_resolved_identity("agent-1", "worker", None);
    let attrs = HashMap::from([
        ("id".to_string(), serde_json::json!("doc-1")),
        (
            "sessionId".to_string(),
            serde_json::json!("approved-session"),
        ),
    ]);

    let decision = engine.authorize(&ctx, "read", "Doc", &attrs);
    assert!(
        !decision.is_allowed(),
        "a resource sessionId must not satisfy a request-session condition"
    );
}

#[test]
fn resource_attributes_remain_available_on_resource_entity() {
    let policy = r#"
        permit(principal is Agent, action == Action::"read", resource is Doc)
        when { resource.classification == "public" };
    "#;
    let engine = AuthzEngine::new(policy).unwrap();
    let ctx = SecurityContext::from_resolved_identity("agent-1", "worker", None);
    let attrs = HashMap::from([
        ("id".to_string(), serde_json::json!("doc-1")),
        ("classification".to_string(), serde_json::json!("public")),
    ]);

    let decision = engine.authorize(&ctx, "read", "Doc", &attrs);
    assert!(
        decision.is_allowed(),
        "ordinary resource state must remain available through resource.*: {decision:?}"
    );
}

#[test]
fn colliding_principal_resource_uid_preserves_principal_authority() {
    let policy = r#"
        permit(principal is Agent, action == Action::"read", resource is Agent)
        when {
            principal.agent_type == "trusted-worker" &&
            principal.agentTypeVerified == true
        };
    "#;
    let engine = AuthzEngine::new(policy).unwrap();
    let ctx = SecurityContext::from_headers(&[
        ("X-Temper-Principal-Id".to_string(), "attacker".to_string()),
        ("X-Temper-Principal-Kind".to_string(), "agent".to_string()),
    ])
    .with_agent_context(Some("attacker"), None, Some("untrusted-worker"));
    let attrs = HashMap::from([
        ("id".to_string(), serde_json::json!("attacker")),
        (
            "agent_type".to_string(),
            serde_json::json!("trusted-worker"),
        ),
        ("agentTypeVerified".to_string(), serde_json::json!(true)),
    ]);

    let decision = engine.authorize(&ctx, "read", "Agent", &attrs);
    assert!(
        !decision.is_allowed(),
        "resource fields must not overwrite authority when principal and resource UIDs match"
    );
}
